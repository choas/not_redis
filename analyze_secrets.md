# Security Analysis Report: not_redis

**Date:** 2026-04-05
**Scope:** Hardcoded secrets, insecure dependencies, injection vulnerabilities, unsafe code patterns

---

## Critical Severity

### 1. Hardcoded Credential Key Committed to Git

- **File:** `.beads/.beads-credential-key`
- **Description:** A binary credential key file is tracked in git history. Any user with repository access (including forks and clones) can retrieve this credential from the commit history even after it is removed from the working tree.
- **Commits containing this file:**
  - `2f1fdd3` — Merge pull request #3
  - `649eb89` — `bd init: initialize beads issue tracking`
- **Risk:** Full credential compromise. Secrets in git history persist indefinitely unless the history is rewritten.
- **Remediation:**
  1. Immediately revoke the credential and generate a new one.
  2. Add `.beads/.beads-credential-key` to `.gitignore` (done in this patch).
  3. Use [BFG Repo-Cleaner](https://rtyley.github.io/bfg-repo-cleaner/) or `git-filter-repo` to purge the file from all historical commits.
  4. Force-push the cleaned history and notify all collaborators to re-clone.

---

## High Severity

### 2. Mutex `.lock().unwrap()` — Panics on Poisoned Locks

Calling `.lock().unwrap()` on a `Mutex` will panic if the mutex was poisoned by a prior panic in another thread. In a server process, this cascading panic can crash the entire application.

| File | Line(s) | Code |
|------|---------|------|
| `src/lib.rs` | 184, 189, 197, 240 | `self.expirations.lock().unwrap()` |
| `src/lib.rs` | 1679, 1685 | `mgr.expirations.lock().unwrap()` (tests) |
| `src/storage/expire.rs` | 27, 33, 48, 74 | `self.expirations.lock().unwrap()` |

- **Risk:** A single panicking thread poisons the mutex, then every subsequent access panics, taking down the server.
- **Remediation:** Use `.lock().unwrap_or_else(|poisoned| poisoned.into_inner())` to recover from poisoned locks, or at minimum use `.lock().expect("expirations mutex poisoned")` for better diagnostics.

---

### 3. `panic!()` in Non-Test Production-Adjacent Code

| File | Line(s) | Pattern |
|------|---------|---------|
| `src/lib.rs` | 1428, 1439, 1469, 1480 | `_ => panic!("expected stream")` |

- **Risk:** While currently inside `#[cfg(test)]` blocks, `panic!` with generic messages makes failure diagnosis difficult and sets a pattern that can leak into production code.
- **Remediation:** Replace with `assert!()` / `assert_eq!()` or `unreachable!()` with descriptive messages.

---

## Medium Severity

### 4. Unchecked `i64` to `usize`/`u64` Casts

Casting a negative `i64` to an unsigned type wraps around to a very large positive value, which can cause out-of-bounds access or excessive memory allocation.

| File | Line(s) | Code |
|------|---------|------|
| `src/types/from_redis_value.rs` | 63 | `Ok(n as u64)` |
| `src/types/from_redis_value.rs` | 77 | `Ok(n as usize)` |
| `src/client.rs` | 1503, 1514 | `Ok(val as usize)` |

- **Risk:** A malicious or malformed Redis response containing a negative integer would wrap to `usize::MAX`, potentially causing:
  - Buffer over-allocation (denial of service)
  - Logic errors in indexing
- **Remediation:**
  ```rust
  let val: i64 = /* ... */;
  usize::try_from(val).map_err(|_| RedisError::InvalidArgument("negative value".into()))
  ```

### 5. Potential Allocation Overflow

- **File:** `src/lib.rs`, line 1000
- **Code:** `Vec::with_capacity(h.len() * 2)`
- **Risk:** If `h.len()` is very large, the multiplication can overflow (wrapping to a small value), or allocate excessive memory causing OOM/DoS.
- **Remediation:** Use `h.len().checked_mul(2).unwrap_or(h.len())` or cap the capacity.

### 6. Integer Overflow in Type Conversions

| File | Line(s) | Code |
|------|---------|------|
| `src/types/to_redis_args.rs` | 61, 67 | `*self as i64` (from `usize`/`isize`) |
| `src/types/value.rs` | 45 | `Value::Int(n as i64)` (from `u64`) |

- **Risk:** On 64-bit systems, values larger than `i64::MAX` silently overflow when cast, producing incorrect negative values in the protocol layer.
- **Remediation:** Use `i64::try_from()` and propagate errors.

---

## Low Severity

### 7. Pervasive `.unwrap()` in Benchmark and Test Code

- **File:** `src/bin/hgetall_bench.rs` — lines 17, 25, 35, 61, 67, 71, 81
- **File:** `src/lib.rs` — lines 1426, 1437, 1467, 1478, 1601, 1604, 1621, 1626, 1644

- **Risk:** Low in test/bench contexts, but panics without context make debugging harder.
- **Remediation:** Replace `.unwrap()` with `.expect("descriptive message")`.

---

## Informational

### 8. Dependency Review

All dependencies from `Cargo.toml` were reviewed:

| Crate | Version | Status |
|-------|---------|--------|
| `tokio` | 1.0 (full features) | Actively maintained, no known vulnerabilities |
| `dashmap` | 6.0 | Safe concurrent HashMap |
| `arc-swap` | 1.7 | Safe atomic pointer swapping |
| `thiserror` | 2.0 | Derive macro only, no runtime risk |
| `rustc-hash` | 2.0 | Non-cryptographic hash (FxHash) — acceptable for HashMaps, must not be used for security |
| `rand` | 0.8 | Provides CSPRNG; not currently used for security-sensitive operations |
| `smallvec` | 1.11 | Stack-allocated vectors, safe |

**Positive findings:**
- `.cargo/deny.toml` is configured with yanked/advisory checks (good supply chain hygiene).
- No `unsafe` blocks were found in application code.
- No SQL or shell command injection vectors were identified (pure in-memory data store).

---

## Summary

| Severity | Count | Key Finding |
|----------|-------|-------------|
| Critical | 1 | Credential key committed to git |
| High | 2 | Mutex poison panics; panic in tests |
| Medium | 3 | Unchecked integer casts; allocation overflow |
| Low | 1 | Unwrap without context in tests/benchmarks |
| Informational | 1 | Dependencies are healthy |

### Immediate Actions Required

1. **Revoke** the `.beads-credential-key` and rotate to a new credential.
2. **Purge** the credential from git history with `git-filter-repo` or BFG Repo-Cleaner.
3. **Fix** `.lock().unwrap()` calls in production code paths to handle poisoned mutexes.
4. **Add** bounds checking on all `i64` → unsigned type casts at system boundaries.
