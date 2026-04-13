# Unit Test Plan: hardy-async

| Document Info | Details |
| ----- | ----- |
| **Functional Area** | Async Runtime Abstraction |
| **Module** | `hardy-async` |
| **Parent Plan** | N/A — internal infrastructure, no LLR traceability |
| **Test Suite ID** | UTP-ASYNC-01 |

## 1. Introduction

This document details the unit test cases for the `hardy-async` module. This crate provides runtime-agnostic async primitives used throughout Hardy — task pools, synchronisation primitives, signal handling, and time utilities. Tests target the correctness of lifecycle management, concurrency limiting, and synchronisation semantics.

## 2. Test Cases

### 2.1 TaskPool Lifecycle

*Objective: Verify task spawning, cancellation tokens, and graceful shutdown.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Spawn and Shutdown** | Spawn a cancellation-aware task, shutdown pool, verify task was cancelled. | `task_pool.rs` | `TaskPool::new()`, spawn, `shutdown().await` | Task's cancel token is triggered; shutdown completes. |
| **Child Token Independence** | Cancel a child token without affecting the parent pool. | `task_pool.rs` | `child_token()`, cancel child | Child cancelled; parent pool still operational. |
| **Parent Cancels Child** | Shutdown parent pool and verify child token is also cancelled. | `task_pool.rs` | Spawn on parent, `shutdown().await` | Both parent and child cancel tokens triggered. |

### 2.2 BoundedTaskPool Concurrency

*Objective: Verify semaphore-based concurrency limiting.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Concurrency Limit** | Spawn 10 tasks with limit 2, verify max concurrent never exceeds 2. | `bounded_task_pool.rs` | `BoundedTaskPool::new(2)`, spawn 10 tasks with AtomicUsize counter | Peak concurrency == 2. |
| **Default Parallelism** | Default constructor uses `available_parallelism`. | `bounded_task_pool.rs` | `BoundedTaskPool::default()`, spawn 1 task | Task completes successfully. |
| **Shutdown Completion** | Spawn 4 cancellation-aware tasks, shutdown, verify all completed. | `bounded_task_pool.rs` | `BoundedTaskPool::new(2)`, spawn 4, `shutdown().await` | All 4 tasks complete; all cancel tokens triggered. |

### 2.3 sync::Mutex

*Objective: Verify async-aware mutex semantics (wraps `tokio::sync::Mutex` or `std::sync::Mutex`).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Basic Lock** | Lock, read, modify value. | `sync/mod.rs` | `Mutex::new(42)` | Value readable and writable through guard. |
| **HashMap Through Mutex** | Insert and lookup through mutex-protected HashMap. | `sync/mod.rs` | `Mutex::new(HashMap::new())` | Entries inserted and retrieved correctly. |
| **Try Lock** | Non-blocking lock attempt, held vs available. | `sync/mod.rs` | Lock held, try_lock | Returns `None` when held, `Some` when available. |
| **Into Inner** | Consume mutex, extract value. | `sync/mod.rs` | `Mutex::new(99)`, `into_inner()` | Returns 99. |
| **Get Mut** | Mutable reference via exclusive access. | `sync/mod.rs` | `Mutex::new(0)`, `get_mut()` | Value modifiable through `&mut`. |

### 2.4 sync::RwLock

*Objective: Verify reader-writer lock semantics.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Basic Read/Write** | Multiple readers, exclusive writer. | `sync/mod.rs` | `RwLock::new(0)`, read + write | Concurrent reads OK; write exclusive. |
| **Try Locks** | Non-blocking read/write attempts, mutual exclusion. | `sync/mod.rs` | `try_read()`, `try_write()` with locks held | Correct mutual exclusion behaviour. |
| **Into Inner** | Consume rwlock, extract value. | `sync/mod.rs` | `RwLock::new(42)`, `into_inner()` | Returns 42. |
| **Get Mut** | Mutable reference via exclusive access. | `sync/mod.rs` | `RwLock::new(0)`, `get_mut()` | Value modifiable through `&mut`. |

### 2.5 sync::spin::Mutex and sync::spin::RwLock

*Objective: Verify spin-based lock semantics (for O(1) critical sections).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Spin Mutex Basic** | Lock, read, modify value. | `sync/spin.rs` | `spin::Mutex::new(42)` | Value readable and writable through guard. |
| **Spin Mutex HashMap** | Insert and lookup through spin-protected HashMap. | `sync/spin.rs` | `spin::Mutex::new(HashMap::new())` | Entries correct. |
| **Spin Mutex Try Lock** | Non-blocking lock attempt. | `sync/spin.rs` | Lock held, try_lock | Returns `None` when held. |
| **Spin RwLock Basic** | Read and write locks. | `sync/spin.rs` | `spin::RwLock::new(0)` | Read/write semantics correct. |
| **Spin RwLock Try Locks** | Non-blocking read/write attempts. | `sync/spin.rs` | `try_read()`, `try_write()` | Correct mutual exclusion. |

### 2.6 sync::spin::Once

*Objective: Verify one-time initialisation cell semantics.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Basic Init** | Initialise, verify state transitions and value access. | `sync/spin.rs` | `Once::new()`, `call_once()`, `get()` | Value set once, accessible after init. |
| **Idempotent Init** | Second `call_once` does not overwrite. | `sync/spin.rs` | `call_once(10)`, `call_once(20)`, `get()` | Returns 10 (first value). |
| **Default** | Default constructor creates uninitialised cell. | `sync/spin.rs` | `Once::default()`, `get()` | Returns `None`. |
| **Debug Format** | Debug output for uninitialised and initialised states. | `sync/spin.rs` | `format!("{:?}", once)` | Shows `None` or `Some(value)`. |

### 2.7 Modules Tested Indirectly

These modules are thin wrappers or type aliases with no independent logic to unit-test. They are verified at the system level by consuming crates.

| Module | Verified By |
| ----- | ----- |
| `spawn!` macro | All crates that spawn tasks (bpa, proto, tvr, bpa-server) |
| `signal::listen_for_cancel` | Server binaries (bpa-server, tcpclv4-server, tvr) |
| `notify::Notify` | BPA channel state machine, proto RpcProxy |
| `time::sleep` | BPA channel tests, pipeline latency test |
| `CancellationToken` (type alias) | TaskPool tests (§2.1) |
| `JoinHandle` (type alias) | TaskPool and BoundedTaskPool tests (§2.1, §2.2) |

## 3. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-async`

* **Pass Criteria:** All 24 tests must return `ok`.

* **Feature Requirements:** Tests in §2.1–2.2 require `tokio` feature. Tests in §2.3–2.4 require `std` feature. Tests in §2.5–2.6 are `no_std` compatible.
