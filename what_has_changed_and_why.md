# What Has Changed and Why

A comprehensive overview of all changes made to the **not_redis** fork — what was improved, why each change was made, and the impact on the project.

---

## Project Summary

**not_redis** is a Rust in-memory data structure library that provides a Redis-compatible API without networking overhead. Instead of connecting to a Redis server over TCP, applications use not_redis as a direct in-process library, eliminating serialization, deserialization, and round-trip latency entirely.

The fork evolved the project from its initial scaffolding into a mature, well-documented, and highly optimized library through **57 commits** across several phases of development.

---

## 1. Core Library Implementation

**What changed:** Built the entire library from scratch in `src/lib.rs` (~1,941 lines), implementing 22 Redis-compatible commands.

**Supported commands:**

| Category | Commands |
|----------|----------|
| Strings | SET, GET, DEL, EXISTS, EXPIRE, TTL, PERSIST |
| Hashes | HSET, HGET, HGETALL, HDEL |
| Lists | LPUSH, RPUSH, LLEN |
| Sets | SADD, SMEMBERS |
| Streams | XADD, XLEN, XTRIM, XDEL, XRANGE, XREVRANGE |
| Utilities | PING, ECHO, DBSIZE, FLUSHDB |

**Key types:**
- `Client` — the main API surface, all methods are async for Redis API compatibility
- `StorageEngine` — concurrent data store backed by DashMap with per-shard locking
- `Value` — RESP protocol type system (Null, Int, String, Array, Map, Set, Bool, Okay)
- `RedisData` — internal storage types (String, List, Set, Hash, ZSet, Stream)
- `StoredValue` — wraps RedisData in Arc with optional TTL expiration
- `ExpirationManager` — background TTL sweeper with bidirectional index

**Why:** The goal was to provide a drop-in replacement for Redis in scenarios where the data doesn't need to leave the process — such as caching, session management, rate limiting, or inter-task communication within a single application. This removes the overhead of network I/O, RESP serialization, and connection pooling, yielding 100-1000x speedups over actual Redis for in-process workloads.

---

## 2. Concurrency Architecture

**What changed:** Chose DashMap (sharded concurrent hash map) as the core data structure instead of a global RwLock or Mutex.

**Why:** DashMap uses fine-grained per-shard locking, so concurrent readers and writers on different keys never block each other. This is critical for multi-threaded async applications where many tasks access the store simultaneously. A global lock would become a bottleneck under contention.

**Design decisions:**
- `StoredValue.data` is wrapped in `Arc<RedisData>` — cheap cloning for reads, copy-on-write for mutations
- Expiration uses a single `Mutex<ExpirationState>` — acceptable because expiration operations are infrequent compared to data operations
- FxBuildHasher replaces the default SipHash — faster for application-controlled keys where DoS resistance is not needed

---

## 3. Performance Optimizations

### 3a. FxHashMap Adoption (commit `181ffc2`)

**What changed:** Replaced all standard `HashMap` and `HashSet` instances with `FxHashMap` and `FxHashSet` from the `rustc-hash` crate.

**Why:** FxHash is significantly faster than SipHash (Rust's default) for short keys. Since not_redis keys are application-controlled (not attacker-controlled), the faster but non-cryptographic hash function is safe and yields measurable throughput gains.

### 3b. Clone Elimination (commit `890b045`)

**What changed:** Eliminated unnecessary `.clone()` calls in hot paths throughout the library.

**Why:** Each clone of a String or Vec allocates heap memory. In tight loops (batch SET/GET), these allocations add up. Moving or borrowing data instead of cloning avoids this overhead.

### 3c. MaxMemory Support (commits `3af5095`, `5a0300e`)

**What changed:** Added configurable memory limits with eviction capabilities, plus a check to skip memory enforcement when no limit is set.

**Why:** Without memory limits, an application could exhaust system memory. MaxMemory support provides a safety net, similar to Redis's `maxmemory` directive. The optimization to skip checks when disabled ensures zero overhead for users who don't need this feature.

### 3d. Memory Compaction (commit `11c1d30`, PR #1)

**What changed:** Implemented automatic memory compaction using a high-water mark strategy. When the current entry count drops below 25% of the peak count, `shrink_to_fit()` is called on the DashMap to release unused memory.

**Why:** DashMap (like most hash maps) doesn't automatically shrink when entries are removed. After a burst of writes followed by deletions, the map retains its expanded memory. The high-water mark approach detects this situation and reclaims memory without penalizing normal operation.

### 3e. SmallVec for ToRedisArgs (commit `022895e`)

**What changed:** Changed `ToRedisArgs` to return `SmallVec<[Value; 1]>` instead of `Vec<Value>`.

**Why:** Most Redis commands pass a single argument at a time. SmallVec stores up to 1 element inline on the stack, avoiding a heap allocation entirely for the common case. Only commands with multiple arguments fall back to heap storage.

### 3f. DashMap Entry API (commit `494a853`)

**What changed:** Refactored `StorageEngine::set()` to use DashMap's entry API instead of separate `get()` + `insert()` calls.

**Why:** The entry API performs a single hash lookup and acquires the shard lock once instead of twice. This halves the lock acquisitions per SET operation and eliminates a potential race condition between the lookup and insertion.

### 3g. Key Ownership Optimization (commits `a85f665`, `0fdc7cb`, `cf0a16a`)

**What changed:**
- `key_to_string` uses `String::from_utf8` for zero-copy ownership transfer when the input is valid UTF-8
- Key-accepting methods (`set`, `get`, `hset`, `hget`, `lpush`, `rpush`, `sadd`) accept `Into<String>` instead of `ToRedisArgs`

**Why:** When callers already have a `String`, the old `ToRedisArgs` conversion would serialize it to bytes and then parse it back — a pointless round-trip. `Into<String>` lets owned strings move directly into the storage engine with zero allocation.

### 3h. ExpirationManager Reverse Index (commit `493edea`)

**What changed:** Added a `key_to_time: FxHashMap<String, Instant>` reverse index to ExpirationManager, alongside the existing `time_to_keys: BTreeMap<Instant, FxHashSet<String>>`.

**Why:** Cancelling a key's expiration (on overwrite or PERSIST) previously required scanning all entries in the BTreeMap — O(n) in the number of scheduled expirations. The reverse index makes this O(1). This matters when many keys have TTLs.

### 3i. Atomic Entry Counter (commit `493edea`)

**What changed:** Replaced `DashMap::len()` with an `AtomicUsize` counter that is incremented on insert and decremented on delete.

**Why:** `DashMap::len()` iterates all shards and briefly locks each one — O(shards) with lock overhead. The atomic counter provides O(1) DBSIZE queries with zero locking. This is especially important under high concurrency.

### 3j. Stream Operation Optimizations (commit `493edea`)

**What changed:**
- XRANGE/XREVRANGE use `.take(count)` to short-circuit iteration instead of collecting all entries and truncating
- XREVRANGE iterates in reverse directly instead of collecting forward and reversing
- XDEL uses `FxHashSet<&[u8]>` for O(1) ID lookups instead of `Vec::contains()` which is O(n)

**Why:** These are algorithmic improvements. The old XRANGE would scan the entire stream even when only 10 entries were requested. The old XDEL performed O(n*m) comparisons (n stream entries times m IDs to delete). These changes reduce both time and memory complexity.

### 3k. Single-Lookup Mutations (commit `493edea`)

**What changed:** HSET, HDEL, LPUSH, RPUSH, SADD now use a single `get_mut()` call to both check for expiration and perform the mutation.

**Why:** Previously, these operations performed one lookup to check if the key existed/was expired, then a second lookup to mutate it. Combining them into one halves the shard lock acquisitions.

---

## 4. Benchmark Infrastructure

### 4a. Core Benchmarks (commit `f21fb9f`)

**What changed:** Implemented a comprehensive Criterion-based benchmark suite in `benches/benchmarks.rs` covering string operations, hash operations, list operations, set operations, and mixed workloads.

**Why:** Without benchmarks, optimizations are guesswork. Criterion provides statistical rigor — confidence intervals, outlier detection, and regression comparisons between runs.

### 4b. Redis Baseline (commits `73172ec`, `78956c0`)

**What changed:** Created `benches/redis_baseline.rs` that runs the same operations against an actual Redis server for direct comparison.

**Why:** Claiming "faster than Redis" requires proof. The baseline benchmarks show not_redis achieves **277x to 989x speedup** over Redis for equivalent operations, validating the fundamental architectural advantage of in-process access.

### 4c. Granular Benchmark Suite (PR #2, commit `2ce6e81`)

**What changed:** Split the monolithic benchmark file into 11 focused benchmark targets:
- Individual data type benchmarks (string, hash, list, set)
- Concurrency-specific benchmarks (per-type and mixed)
- Contention benchmarks (multiple tasks hitting the same keys)
- Throughput benchmarks (operations per second)
- Autoresearch metric (quick mixed workload baseline)

**Why:** Granular benchmarks allow targeted optimization. When investigating hash performance, running only `cargo bench --bench bench_hash` takes seconds instead of minutes. This was designed for AI-guided automated performance research (pi-autoresearch), where an agent iterates through optimization experiments.

### 4d. CI Benchmark Enforcement (`.github/workflows/benchmark.yml`)

**What changed:** Added a GitHub Actions workflow that runs benchmarks on every PR and compares against the main branch. Any benchmark regressing by more than 5% fails the PR.

**Why:** Performance regressions are easy to introduce and hard to notice. Automated enforcement ensures that no PR accidentally degrades throughput. The 5% threshold allows for measurement noise while catching real regressions.

---

## 5. Documentation

### 5a. Diataxis Documentation Framework (commit `1ee50f9`)

**What changed:** Created four documentation files following the Diataxis framework:

- **`docs/tutorial.md`** — Step-by-step guide for new users, covering client creation, all data types, error handling, and a complete session management example
- **`docs/how-to.md`** — Task-oriented recipes: caching, session stores, async task communication, streams as event logs, rate limiting, visitor tracking
- **`docs/reference.md`** — Complete API reference with tables for all commands, types, traits, and error variants
- **`docs/explanation.md`** — Architecture rationale: why not_redis exists, concurrency model, memory management, performance analysis, and comparison with similar crates

**Why:** Good documentation serves four distinct needs: learning (tutorials), doing (how-to guides), looking up (reference), and understanding (explanation). The Diataxis framework addresses all four. This makes the library accessible to newcomers while providing depth for advanced users.

### 5b. Architecture Analysis (commit `ddaa000`)

**What changed:** Created `analyze_architecture.md` — a 499-line structural analysis of the entire crate, covering module organization, dependency usage, type system design, concurrency patterns, test coverage, and 7 identified structural issues.

**Why:** Understanding the architecture is a prerequisite for making informed improvements. The analysis revealed that the project has ~2,878 lines of orphaned code in `src/storage/`, `src/types/`, `src/client.rs`, and `src/error.rs` that are not compiled into the library (all logic lives in `src/lib.rs`). This informs future refactoring decisions.

### 5c. Security Audit (commit `e99bef4`)

**What changed:** Created `analyze_secrets.md` documenting security findings:
- **Critical:** `.beads/.beads-credential-key` committed to git history
- **High:** `Mutex::lock().unwrap()` can panic on poisoned mutexes
- **Medium:** Unchecked integer casts and potential allocation overflows
- **Low:** Benchmark code uses `.unwrap()` without context

**Why:** Security issues compound over time. Documenting them ensures they're tracked and addressed, even if not fixed immediately.

### 5d. Dead Code Analysis (commit `b5905a3`)

**What changed:** Created `analyze_dead_code.md` identifying unused code: 2,878 lines of stale modules, unused enum variants, unused public methods, unreachable branches, and unused dependencies (`arc-swap`, `rand` in library code).

**Why:** Dead code increases maintenance burden and confuses new contributors. Documenting it is the first step toward cleanup.

### 5e. Performance Analysis (commit `7e6e36e`)

**What changed:** Created `analyze_performance.md` documenting all 6 optimizations from commit `493edea` with before/after complexity analysis and regression risk assessment.

**Why:** Performance changes need documentation so future contributors understand why the code is structured the way it is and don't accidentally revert optimizations.

### 5f. Contributing Guide (commits `7041c69`, `b0a1fe4`)

**What changed:** Created `CONTRIBUTING.md` with performance requirements:
- No benchmark may regress by more than 5%
- Automated benchmark comparison on every PR
- Admin override process for justified regressions

**Why:** Clear contribution guidelines prevent performance regressions from being merged and set expectations for contributors.

---

## 6. CI/CD and Tooling

### 6a. GitHub Actions Workflows

**What changed:** Created four workflow files:
- `ci.yml` — Runs `cargo test`, `cargo clippy`, and `cargo fmt --check` on every push
- `benchmark.yml` — Compares PR benchmarks against main with 5% regression threshold
- `enforce-pr.yml` — Enforces PR-based workflow (no direct pushes to main)
- `security.yml` — Scheduled security audit via `cargo deny`

**Why:** Automated CI catches bugs, style violations, and security issues before they reach the main branch. The enforce-PR workflow ensures all changes go through code review.

### 6b. Cargo Deny Configuration (`.cargo/deny.toml`)

**What changed:** Configured license and security policies for dependencies.

**Why:** Ensures all dependencies have compatible licenses and flags known security advisories.

### 6c. Git Hooks (`git_hooks/`)

**What changed:** Added pre-commit (runs `cargo fmt --check` and `cargo clippy`) and commit-msg (validates conventional commit format) hooks.

**Why:** Catches formatting and lint issues before they become commits, keeping the history clean.

### 6d. Agent Orchestration (`AGENTS.md`)

**What changed:** Documented branch strategies and workflows for AI coding agents working on the project.

**Why:** The project uses AI agents (Claude, pi-autoresearch) for automated optimization experiments. Clear guidelines prevent agents from conflicting with each other or with human contributors.

---

## 7. Automated Performance Research (Autoresearch)

**What changed:** Integrated with pi-autoresearch, an AI-guided performance optimization system:
- `autoresearch.jsonl` — Experiment log tracking optimization attempts and results
- `autoresearch_metric.rs` — Quick benchmark for measuring baseline performance
- `get_performance_metric.sh` — Script to extract throughput numbers
- `benchmark_runner.sh` — Script to run and compare benchmarks

**Key experiments logged:**
1. Baseline measurement: 4,600,458 ops/sec
2. FxBuildHasher + Arc clone removal: +2.1%
3. Direct DashMap access in Client::get: +2.3%
4. SmallVec inline storage: +4.1%
5. Value::String SmallVec refactor: +9.3%
6. SmallVec throughout + allocation elimination: total +9.4% vs baseline

**Why:** Manual optimization is slow and biased. Automated research systematically explores the optimization space, measures each change rigorously, and logs results for reproducibility. The autoresearch branch (PR #3) delivered a cumulative 9.4% throughput improvement through 6 incremental experiments.

---

## 8. Testing

**What changed:** Created comprehensive test coverage:
- **9 unit tests** in `src/lib.rs` covering core functionality
- **47 integration tests** in `tests/integration_tests.rs` covering all 22 commands, edge cases, expiration behavior, and concurrent access patterns
- `run_integration_tests.sh` — Convenience script for running integration tests

**Why:** Tests catch regressions and document expected behavior. The integration tests use the public `Client` API, ensuring the library works correctly from a user's perspective.

---

## 9. Performance Results

The cumulative effect of all optimizations:

| Metric | Value |
|--------|-------|
| vs Redis (single ops) | 277x - 989x faster |
| Batch write throughput | 4.6 - 5.2M ops/sec |
| Batch read throughput | 3.1 - 3.3M ops/sec |
| Autoresearch improvement | +9.4% over initial baseline |

These numbers reflect the fundamental advantage of in-process access (no network, no serialization, CPU cache locality) combined with careful algorithmic optimization (FxHash, SmallVec, entry API, atomic counters, reverse indexes).

---

## Summary of Key Decisions

| Decision | Alternative Considered | Why This Choice |
|----------|----------------------|-----------------|
| DashMap over RwLock | Global RwLock<HashMap> | Per-shard locking avoids global contention |
| FxHash over SipHash | Default hasher | Faster for non-adversarial keys |
| Arc<RedisData> | Clone-on-read | Cheap reads via reference counting |
| SmallVec<[Value; 1]> | Vec<Value> | Stack allocation for single-arg commands |
| Atomic entry counter | DashMap::len() | O(1) without shard locking |
| Bidirectional expiration index | Linear scan | O(1) cancel instead of O(n) |
| Monolithic lib.rs | Multi-module split | Evolved organically; refactoring is planned |
| Async API | Sync API | Compatibility with redis-rs traits |
| 5% benchmark regression gate | No gate | Prevents accidental performance loss |
