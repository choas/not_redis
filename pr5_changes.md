# PR #5 Changes Analysis: Performance Optimization Suite

**PR:** https://github.com/cmgriffing/not_redis/pull/5  
**Commit:** `51de61762f9cae7ba3204e7cd58c0b589557b1cb`  
**Claimed result:** +9.4% ops/sec improvement (7.1M -> 7.8M)

---

## Changes We Already Have

### 1. FxHash for DashMap (FxBuildHasher)

**PR change:** Switch `DashMap<String, StoredValue>` to `DashMap<String, StoredValue, FxBuildHasher>`, replacing the default SipHash with the faster (non-cryptographic) FxHash.

**Our status: DONE** (`src/lib.rs:231`)

```rust
// Our code already uses FxBuildHasher:
data: Arc<DashMap<String, StoredValue, FxBuildHasher>>,
data: Arc::new(DashMap::with_hasher(FxBuildHasher)),
```

> Note: Our `src/storage/engine.rs` still uses the default hasher (`DashMap::new()` on line 31, 44). Only the `lib.rs` version has this optimization.

### 2. Direct DashMap access in `Client::get`

**PR change:** Bypass `storage.get()` wrapper and use `storage.data.get()` directly to avoid an extra `Arc` clone. Includes an explicit `drop(stored)` before calling `remove` to avoid DashMap deadlock.

**Our status: DONE** (`src/lib.rs:861-873`)

```rust
// Our code already does direct access with the explicit drop:
if let Some(stored) = self.storage.data.get(&key_str) {
    if stored.is_expired() {
        drop(stored);
        self.storage.remove(&key_str);
        ...
    }
    match &*stored.data { ... }
}
```

---

## Changes We Do NOT Have (Potential Improvements)

### 3. SmallVec<[u8; 64]> for Value::String

**PR change:** Replace `Value::String(Vec<u8>)` with `Value::String(SmallVec<[u8; 64]>)`. Values <= 64 bytes are stored inline on the stack, avoiding heap allocation entirely.

**Our status: NOT IMPLEMENTED** (`src/lib.rs:92`)

```rust
// Our current code:
String(Vec<u8>),

// PR changes to:
String(SmallVec<[u8; 64]>),
```

**Impact:** This is the highest-impact change in the PR. Most Redis keys and values are short strings (< 64 bytes), so this eliminates a large number of heap allocations. Requires updating all `From` trait impls and `FromRedisValue`/`ToRedisArgs` impls.

### 4. SmallVec<[u8; 64]> for RedisData

**PR change:** Replace all `Vec<u8>` containers in `RedisData` with `SmallVec<[u8; 64]>`.

**Our status: NOT IMPLEMENTED** (`src/lib.rs:145-152` and `src/storage/types.rs:12-18`)

```rust
// Our current code:
pub enum RedisData {
    String(Vec<u8>),
    List(VecDeque<Vec<u8>>),
    Set(FxHashSet<Vec<u8>>),
    Hash(FxHashMap<Vec<u8>, Vec<u8>>),
    ZSet(BTreeMap<Vec<u8>, f64>),
    Stream(Vec<StreamEntry>),
}

// PR changes to:
pub enum RedisData {
    String(SmallVec<[u8; 64]>),
    List(VecDeque<SmallVec<[u8; 64]>>),
    Set(FxHashSet<SmallVec<[u8; 64]>>),
    Hash(FxHashMap<SmallVec<[u8; 64]>, SmallVec<[u8; 64]>>),
    ZSet(BTreeMap<SmallVec<[u8; 64]>, f64>),
    Stream(Vec<StreamEntry>),  // unchanged
}
```

**Impact:** Extends the SmallVec optimization to all stored data, not just RESP values. Affects hash fields, set members, list elements, etc. Note that `StreamEntry` is left unchanged (still uses `Vec<u8>`), so conversions with `.into_vec()` are needed at the stream boundary.

> **Caveat:** We have two copies of `RedisData` -- one in `src/lib.rs` and one in `src/storage/types.rs`. Both would need updating.

### 5. Removed unnecessary Arc::clone in StorageEngine::set

**PR change:** In the occupied-entry branch of `set()`, the old value was being fully cloned (`entry.get().clone()`) just to check `expire_at`. Changed to borrow (`entry.get()`).

**Our status: PARTIALLY ADDRESSED** (`src/lib.rs:302-310`)

Our `lib.rs` version already avoids the full clone by using the entry API directly:
```rust
dashmap::mapref::entry::Entry::Occupied(mut entry) => {
    if entry.get().expire_at.is_some() {
        self.expiration.cancel(entry.key());
    }
    entry.insert(StoredValue { ... });
}
```

However, `src/storage/engine.rs` (lines 70-128) uses a different approach with `self.data.get(key).map(|v| v.clone())` to fetch the old value, which does perform a full clone.

### 6. std::str::from_utf8 instead of String::from_utf8 for parsing

**PR change:** In `FromRedisValue` impls for `i64` and `bool`, use `std::str::from_utf8(&s)` instead of `String::from_utf8(s)` to avoid allocating a `String` when only a `&str` is needed for parsing.

**Our status: NOT IMPLEMENTED** (`src/lib.rs:762, 778`)

```rust
// Our current code (i64):
Value::String(s) => String::from_utf8(s)
    .ok()
    .and_then(|s| s.parse().ok())

// PR changes to:
Value::String(s) => std::str::from_utf8(&s)
    .ok()
    .and_then(|s| s.parse().ok())
```

```rust
// Our current code (bool):
Value::String(s) => {
    let s_str = String::from_utf8(s).map_err(|_| RedisError::ParseError)?;
    Ok(s_str == "1" || s_str.eq_ignore_ascii_case("true"))
}

// PR changes to:
Value::String(s) => {
    let s_str = std::str::from_utf8(&s).map_err(|_| RedisError::ParseError)?;
    Ok(s_str == "1" || s_str.eq_ignore_ascii_case("true"))
}
```

**Impact:** Small but easy win -- avoids a heap allocation per parse. Especially relevant for numeric operations (INCR/DECR family).

### 7. value_to_vec returning SmallVec

**PR change:** Change `fn value_to_vec` return type from `Vec<u8>` to `SmallVec<[u8; 64]>` and update all internal conversions.

**Our status: NOT IMPLEMENTED** (`src/lib.rs:1442`)

This is a downstream consequence of change #3. If `Value::String` uses `SmallVec`, then `value_to_vec` naturally returns `SmallVec` too.

---

## Summary

| # | Optimization | Our Status | Effort | Impact |
|---|---|---|---|---|
| 1 | FxBuildHasher for DashMap | Done (lib.rs only) | - | - |
| 2 | Direct DashMap access in Client::get | Done | - | - |
| 3 | SmallVec<[u8;64]> for Value::String | **Not done** | Medium | **High** |
| 4 | SmallVec<[u8;64]> for RedisData | **Not done** | High | **High** |
| 5 | Remove Arc::clone in set() | Partially done | Low | Low |
| 6 | str::from_utf8 instead of String::from_utf8 | **Not done** | Low | Low |
| 7 | value_to_vec returning SmallVec | **Not done** | Medium | Medium |

### Recommended adoption order

1. **Change #6** (str::from_utf8) -- trivial, zero-risk, immediate small gain
2. **Change #5** (Arc::clone in storage/engine.rs) -- small fix in the modular engine
3. **Changes #3 + #4 + #7** (SmallVec migration) -- these should be done together as they are interdependent. This is the bulk of the performance improvement but also the largest change, touching `Value`, `RedisData`, `ToRedisArgs`, `FromRedisValue`, `value_to_vec`, and all tests

### Additional note on our codebase

We have two parallel implementations: the monolithic `src/lib.rs` and the modular `src/storage/` directory. The FxBuildHasher optimization only exists in `lib.rs`. If `src/storage/engine.rs` is the canonical implementation going forward, it also needs the FxBuildHasher change applied.
