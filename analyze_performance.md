# Performance Analysis of not_redis

## Summary

This document describes the performance optimizations applied to `src/lib.rs` and analyzes their expected impact. The changes target six areas: expiration management, entry counting, stream operations, expired-key handling, and iterator efficiency.

---

## Change 1: ExpirationManager — O(1) cancel and schedule via reverse index

**Before:** `ExpirationManager` stored a single `BTreeMap<Instant, FxHashSet<String>>`. Cancelling an expiration required a full scan of every time slot (`retain` over the entire map) because there was no way to look up which `Instant` a key belonged to. Scheduling a key that already had an expiration would silently insert a duplicate entry at a new time without removing the old one.

**After:** A new `ExpirationState` struct holds two maps:
- `time_to_keys: BTreeMap<Instant, FxHashSet<String>>` (same as before)
- `key_to_time: FxHashMap<String, Instant>` (reverse index)

`cancel` is now O(1) average-case (hash lookup + set removal) instead of O(n) where n is the number of distinct expiration time slots. `schedule` removes stale entries before inserting, preventing ghost expirations that could cause unnecessary sweeper work.

**Trade-off:** Slightly higher memory usage (one extra `FxHashMap` entry per key with an expiration). This is negligible for typical workloads and is outweighed by the cancel speedup, which matters most under high churn (frequent SET with EX/PX on the same key).

**Risk of regression:** None. The reverse index is maintained under the same `Mutex`, so no new synchronization cost is introduced. The only overhead is the extra `HashMap` insert/remove per operation, which is O(1) amortized.

---

## Change 2: Atomic `entry_count` replaces `DashMap::len()`

**Before:** `StorageEngine::len()` called `self.data.len()`, which internally iterates over all DashMap shards and sums their lengths — an O(shards) operation that also briefly acquires each shard's read lock. The high-water mark update in `set()` also called `self.data.len()` on every insertion.

**After:** An `AtomicUsize` (`entry_count`) tracks the number of entries. It is incremented on new insertions, decremented on removals (both explicit and sweeper-driven), and reset on `flush`. `len()` is now a single `Relaxed` atomic load — O(1) with no locking.

**Trade-off:** The atomic counter could theoretically drift if there were bugs in the increment/decrement logic, but the code carefully distinguishes new insertions (`is_new`) from overwrites, and checks `data.remove().is_some()` before decrementing. `flush` resets the counter to zero, providing a periodic correction point.

**Impact:** This is the highest-impact change for workloads that frequently call `DBSIZE` or trigger `maybe_compact`, since both previously required a full shard scan. The `set` path also benefits because the high-water mark update no longer requires a shard scan on every call.

---

## Change 3: `set()` avoids cloning the old value

**Before:** In the `Occupied` branch of `set()`, the code cloned the entire old `StoredValue` (`let old = entry.get().clone()`) just to check whether `expire_at` was `Some`. Since `StoredValue` contains an `Arc<RedisData>`, the clone itself is cheap (Arc bump), but it is still an unnecessary atomic ref-count increment and decrement.

**After:** The check is done directly on `entry.get().expire_at` without cloning.

**Impact:** Minor — avoids two atomic operations per overwrite. Measurable only under extreme contention on hot keys.

---

## Change 4: XDEL uses `FxHashSet` for ID lookup

**Before:** `xdel` called `entries.retain(|(id, _)| !entry_ids.contains(&id.as_slice()))`. `entry_ids` is a `Vec<&[u8]>`, so `contains` is O(m) per entry, making the total O(n * m) where n is the stream length and m is the number of IDs to delete.

**After:** The IDs are collected into an `FxHashSet<&[u8]>` first, making each lookup O(1) average-case. Total cost is O(n + m).

**Impact:** Significant when deleting many IDs from a large stream. Negligible for the common case of deleting 1-2 entries.

---

## Change 5: XRANGE / XREVRANGE avoid unnecessary allocation

**Before:** `xrange` collected all matching entries into a `Vec`, then called `truncate(count)`. `xrevrange` called `xrange` (which collected all matches), then reversed the result and truncated.

**After:** `xrange` uses `.take(count)` on the iterator before `.cloned().collect()`, so it never allocates beyond what is needed. `xrevrange` now iterates in reverse directly (`.iter().rev()`) with its own filter and `.take(count)`, avoiding a full forward scan, collection, reverse, and truncation.

**Impact:** Significant for streams with many entries when a small `COUNT` is specified. Memory allocation and cloning are reduced from O(n) to O(count). The `xrevrange` improvement is especially notable since it previously did twice the work (forward scan + reverse).

---

## Change 6: Expired-key handling in HSET, HDEL, LPUSH, RPUSH, SADD — single lookup

**Before:** These commands first called `self.storage.get(&key_str)` (read lock) to check if the key was expired, dropped the lock, removed the key if expired, then called `self.storage.data.get_mut(&key_str)` (write lock) to mutate. This was two separate DashMap lookups.

**After:** A single `self.storage.data.get_mut(&key_str)` is used. If the entry is expired, the mutable reference is dropped and `remove` is called, then a fresh value is inserted. If not expired, the mutation proceeds directly.

**Impact:** Reduces the number of DashMap shard lock acquisitions from 2 to 1 in the common (non-expired) path. Under contention, this halves the lock pressure for these commands.

**Risk of regression:** The expired-key path now does `drop(stored); self.storage.remove(&key_str)` which re-acquires the lock. This is the same total cost as before for expired keys, but the common path (non-expired) is faster.

---

## Potential Regressions

| Change | Could it be slower? | When? |
|--------|---------------------|-------|
| Reverse index in ExpirationManager | Marginally | If almost no keys have expirations (extra HashMap overhead with no cancel benefit) |
| AtomicUsize entry_count | No | Atomic ops are cheaper than shard scans in all cases |
| Cloned old value removal | No | Strictly removes work |
| FxHashSet for XDEL | Marginally | If always deleting exactly 1 ID (HashSet construction overhead > single Vec scan) |
| XRANGE/XREVRANGE take | No | Strictly reduces allocation |
| Single-lookup expired check | No | Strictly reduces lock acquisitions on the hot path |

**Overall assessment:** No change makes the common-case path slower. The expiration reverse index and XDEL HashSet have trivial overhead for small inputs that is dominated by the improvement on larger inputs. The entry_count change and single-lookup pattern are pure wins.

---

## Recommendations for Further Optimization

1. **Batch expiration cancellation:** When `flush` is called, the expiration manager clears its maps, but keys are also cleared from DashMap independently. These could be coordinated to avoid redundant work.

2. **Sharded expiration state:** The single `Mutex` on `ExpirationState` could become a bottleneck if many threads schedule/cancel expirations concurrently. A sharded approach (e.g., per-key-hash buckets) would reduce contention.

3. **Lazy expiration instead of sweeper:** Instead of (or in addition to) the periodic sweeper, keys could be lazily expired on access. This is already partially done (the `is_expired` checks), but the sweeper still runs on a fixed interval regardless of load.

4. **Stream data structure:** Streams use `Vec<(Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>)>`. For large streams, a `BTreeMap` keyed by ID would allow O(log n) range queries instead of O(n) linear scans.

5. **Avoid `shrink_to_fit` in `compact`:** `DashMap::shrink_to_fit` can be expensive and cause allocation churn. Consider compacting only when the ratio is more extreme (e.g., 8:1 instead of 4:1) or on an explicit user command only.
