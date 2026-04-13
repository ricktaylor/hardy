# hardy-async Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-async` |
| **Standard** | N/A -- internal infrastructure |
| **Test Plans** | [`UTP-ASYNC-01`](unit_test_plan.md) |
| **Date** | 2026-04-13 |

## 1. Functional Coverage Summary

The `hardy-async` crate has no formal LLRs -- it is internal infrastructure. The table below maps functional areas to their verification status. All functional areas verified (10 pass).

| Area | Feature | Result | Test |
| :--- | :--- | :--- | :--- |
| **TaskPool** | Spawn and graceful shutdown | Pass | `task_pool.rs::test_task_pool_spawn_and_shutdown` |
| **TaskPool** | Child token independent cancellation | Pass | `task_pool.rs::test_child_token_independent_cancellation` |
| **TaskPool** | Parent cancellation cascades to child | Pass | `task_pool.rs::test_parent_cancels_child` |
| **BoundedTaskPool** | Concurrency limit enforced | Pass | `bounded_task_pool.rs::test_bounded_pool_limits_concurrency` |
| **BoundedTaskPool** | Default uses available parallelism | Pass | `bounded_task_pool.rs::test_bounded_pool_default_uses_available_parallelism` |
| **BoundedTaskPool** | Graceful shutdown completes all tasks | Pass | `bounded_task_pool.rs::test_bounded_pool_shutdown` |
| **sync::Mutex** | Lock, try\_lock, into\_inner, get\_mut | Pass | `sync/mod.rs::mutex_basic`, `mutex_hashmap`, `mutex_try_lock`, `mutex_into_inner`, `mutex_get_mut` |
| **sync::RwLock** | Read, write, try\_read, try\_write, into\_inner, get\_mut | Pass | `sync/mod.rs::rwlock_basic`, `rwlock_try_locks`, `rwlock_into_inner`, `rwlock_get_mut` |
| **sync::spin::Mutex / RwLock** | Spinlock lock, try\_lock, read, write | Pass | `sync/spin.rs::mutex_basic`, `mutex_hashmap`, `mutex_try_lock`, `rwlock_basic`, `rwlock_try_locks` |
| **sync::spin::Once** | Initialisation, idempotency, default, debug | Pass | `sync/spin.rs::once_basic`, `once_multiple_calls`, `once_default`, `once_debug` |

## 2. Test Inventory

### Unit Tests

24 test functions across 4 source files.

#### TaskPool (`task_pool.rs`) -- 3 tests, `#[tokio::test]`

| Test Function | Scope |
| :--- | :--- |
| `test_task_pool_spawn_and_shutdown` | Spawn a cancellation-aware task, shutdown pool, verify cancellation flag |
| `test_child_token_independent_cancellation` | Cancel child token without affecting parent pool |
| `test_parent_cancels_child` | Shutdown parent pool, verify child token is also cancelled |

#### BoundedTaskPool (`bounded_task_pool.rs`) -- 3 tests, `#[tokio::test]`

| Test Function | Scope |
| :--- | :--- |
| `test_bounded_pool_limits_concurrency` | Spawn 10 tasks with limit 2, verify max concurrent never exceeds 2 |
| `test_bounded_pool_default_uses_available_parallelism` | Default constructor works, spawns and completes a task |
| `test_bounded_pool_shutdown` | Spawn 4 cancellation-aware tasks, shutdown, verify all completed |

#### sync::Mutex / sync::RwLock (`sync/mod.rs`) -- 9 tests, `#[test]`

| Test Function | Scope |
| :--- | :--- |
| `mutex_basic` | Lock, read, write value |
| `mutex_hashmap` | Insert and lookup through mutex-protected HashMap |
| `rwlock_basic` | Multiple concurrent readers, exclusive writer |
| `mutex_try_lock` | Non-blocking lock attempt (held vs available) |
| `rwlock_try_locks` | Non-blocking read/write lock attempts, mutual exclusion |
| `mutex_into_inner` | Consume mutex and extract value |
| `rwlock_into_inner` | Consume rwlock and extract value |
| `mutex_get_mut` | Mutable reference via exclusive access |
| `rwlock_get_mut` | Mutable reference via exclusive access |

#### sync::spin (`sync/spin.rs`) -- 9 tests, `#[test]`

| Test Function | Scope |
| :--- | :--- |
| `once_basic` | Initialise, verify state transitions and value access |
| `once_multiple_calls` | Second `call_once` is idempotent, returns original value |
| `once_default` | Default constructor creates uninitialised cell |
| `once_debug` | Debug formatting for uninitialised and initialised states |
| `mutex_basic` | Spinlock lock, read, write value |
| `mutex_hashmap` | Insert and lookup through spinlock-protected HashMap |
| `rwlock_basic` | Multiple concurrent readers, exclusive writer |
| `mutex_try_lock` | Non-blocking lock attempt (held vs available) |
| `rwlock_try_locks` | Non-blocking read/write lock attempts, mutual exclusion |

### Modules Without Tests

| Module | Reason |
| :--- | :--- |
| `spawn.rs` | Macro -- exercised indirectly by `signal.rs` and all consuming crates |
| `signal.rs` | Requires OS signal delivery; verified at system level by server binaries |
| `cancellation_token.rs` | Type alias -- tested through TaskPool tests |
| `join_handle.rs` | Type alias -- tested through TaskPool and BoundedTaskPool tests |
| `notify.rs` | Thin wrapper -- tested by consuming crates (proto, bpa) |
| `time.rs` | Thin wrapper -- tested by consuming crates |

## 3. Coverage vs Plan

Cross-reference against [`UTP-ASYNC-01`](unit_test_plan.md):

| Area | Tests | Status |
| :--- | :--- | :--- |
| TaskPool lifecycle (spawn, cancel, shutdown) | 3 | Complete |
| Hierarchical cancellation | 2 | Complete |
| Bounded concurrency | 3 | Complete |
| sync::Mutex | 5 | Complete |
| sync::RwLock | 5 | Complete |
| sync::spin::Mutex | 4 | Complete |
| sync::spin::RwLock | 2 | Complete |
| sync::spin::Once | 4 | Complete |
| Signal handling | 0 | System-level only |
| spawn! macro | 0 | Indirect (via consumers) |
| Notify | 0 | Indirect (via consumers) |
| time::sleep | 0 | Indirect (via consumers) |
| **Total** | **24** | |

## 4. Line Coverage

```
cargo llvm-cov test --package hardy-async --lcov --output-path lcov.info --html
lcov --summary lcov.info
```

Results (2026-04-13, from workspace-wide run):

```
  lines......: 85.6% (379 of 443 lines)
  functions..: 11.5% (97 of 841 functions)
```

Line coverage is for production code only (test modules excluded). Function count is inflated by generic monomorphisation.

Per-file breakdown:

| File | Covered | Total | Coverage | Notes |
| :--- | :--- | :--- | :--- | :--- |
| `lib.rs` | 3 | 3 | 100% | Re-exports only |
| `sync/mod.rs` | 102 | 105 | 97% | Mutex + RwLock wrappers |
| `bounded_task_pool.rs` | 138 | 142 | 97% | Semaphore-based pool |
| `sync/spin.rs` | 112 | 124 | 90% | Spin-based Mutex, RwLock, Once |
| `task_pool.rs` | 66 | 76 | 87% | Core task pool + cancel token |
| `notify.rs` | 0 | 12 | 0% | Thin tokio::sync::Notify wrapper — no unit tests |
| `signal.rs` | 0 | 19 | 0% | SIGTERM/Ctrl+C handler — requires process signals |
| `time.rs` | 0 | 8 | 0% | Thin tokio::time::sleep wrapper — no unit tests |

## 5. Test Infrastructure

Tests use straightforward inline `#[test]` and `#[tokio::test]` modules within their respective source files. No external test helpers, fixtures, or mock types are used.

- TaskPool and BoundedTaskPool tests require the `tokio` feature (gated with `#[cfg(all(test, feature = "tokio"))]`)
- sync::Mutex / sync::RwLock tests require the `std` feature (gated with `#[cfg(all(test, feature = "std"))]`)
- sync::spin tests are unconditional (`#[cfg(test)]`)
- BoundedTaskPool concurrency test uses `AtomicUsize` with CAS to track maximum concurrent tasks

## 6. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Signal handling | No unit tests | Low | Requires OS signal delivery; verified by server binary integration tests |
| spawn! macro | No direct tests | Low | Macro is exercised by `signal.rs` and all consuming crates |
| Notify | No unit tests | Low | Thin wrapper over `tokio::sync::Notify`; tested by consumers |
| time::sleep | No unit tests | Low | Thin wrapper with trivial logic; edge cases (negative duration) are simple branches |
| Panic abort behaviour | Not tested | Low | TaskPool aborts on task panic; testing would require process-level harness |

## 7. Conclusion

The `hardy-async` crate has 24 unit tests with 85.6% line coverage (379/443 lines). Core modules are well-covered: `bounded_task_pool.rs` (97%), `sync/mod.rs` (97%), `sync/spin.rs` (90%), `task_pool.rs` (87%). The untested modules (`notify.rs`, `signal.rs`, `time.rs`) are thin wrappers over tokio primitives exercised at the system level by consuming crates. The primary strength is thorough verification of the shutdown and concurrency-limiting semantics that are critical to Hardy's graceful shutdown behaviour.
