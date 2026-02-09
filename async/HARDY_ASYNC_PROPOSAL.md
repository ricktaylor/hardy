# Hardy-Async Crate Status

**Last Updated:** 2026-02-09

## Overview

The `hardy-async` crate provides runtime-agnostic async primitives for the Hardy DTN implementation, enabling future support for both Tokio (cloud/server) and Embassy (embedded/no_std) runtimes.

**Status:** Phase 1 Complete, Phase 2 In Progress

---

## What's Been Implemented

### 1. TaskPool

**Location:** `/workspace/async/src/task_pool.rs`

**Status:** Fully implemented and migrated across entire workspace

**API:**

```rust
pub struct TaskPool {
    cancel_token: CancellationToken,
    task_tracker: TaskTracker,
}

impl TaskPool {
    pub fn new() -> Self;
    pub fn cancel_token(&self) -> &CancellationToken;
    pub fn child_token(&self) -> CancellationToken;
    pub fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>;
    pub async fn shutdown(&self);
    pub fn is_cancelled(&self) -> bool;
}
```

**Migrations Completed:**

- `bpa` crate - Store, Dispatcher, RIB modules
- `bpa` CLA Registry - Uses Registry-owned TaskPool for all operations
- `bpa` Service Registry - Uses Registry-owned TaskPool for cleanup
- `tcpclv4` crate - Main Cla struct uses `Arc<TaskPool>`
- `file-cla` crate - Main Cla struct uses TaskPool
- `proto/proxy` - RpcProxy now has proper lifecycle management

---

### 2. spawn! Macro

**Location:** `/workspace/async/src/spawn.rs`

**Status:** Fully implemented with tracing support

**API:**

```rust
// Simple case (no fields):
hardy_async::spawn!(pool, "task_name", async { ... })

// Complex case (with span fields - use parentheses):
hardy_async::spawn!(pool, "task_name", (?field1, field2 = value), async { ... })
```

**Features:**

- Automatic tracing instrumentation when `tracing` feature enabled
- Proper span following for distributed tracing
- No-op when tracing disabled (zero overhead)

---

### 3. BoundedTaskPool

**Location:** `/workspace/async/src/bounded_task_pool.rs`

**Status:** Fully implemented

**API:**

```rust
pub struct BoundedTaskPool {
    inner: TaskPool,
    semaphore: Arc<Semaphore>,
}

impl BoundedTaskPool {
    pub fn new(max_concurrent: usize) -> Self;
    pub async fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>;
    pub fn cancel_token(&self) -> &CancellationToken;
    pub async fn shutdown(&self);
}

impl Default for BoundedTaskPool {
    fn default() -> Self {
        // Uses available_parallelism()
    }
}
```

**Purpose:** Encapsulates the Semaphore + TaskPool pattern for bounded parallelism, hiding runtime-specific primitives.

**Usage in BPA:** `dispatch.rs:poll_waiting()` uses `BoundedTaskPool::default()` for parallel bundle processing.

---

### 4. JoinHandle Type Alias

**Location:** `/workspace/async/src/join_handle.rs`

**Status:** Implemented

**API:**

```rust
#[cfg(feature = "tokio")]
pub type JoinHandle<T> = tokio::task::JoinHandle<T>;

// Future: Embassy equivalent
```

**Purpose:** Prepares for Embassy runtime abstraction by centralizing the JoinHandle type.

---

### 5. CancellationToken Type Alias

**Location:** `/workspace/async/src/cancellation_token.rs`

**Status:** Implemented

**API:**

```rust
#[cfg(feature = "tokio")]
pub type CancellationToken = tokio_util::sync::CancellationToken;

// Future: Embassy equivalent
```

**Purpose:** Centralizes the CancellationToken type for runtime abstraction. Used throughout TaskPool and BoundedTaskPool.

---

### 6. Notify Wrapper

**Location:** `/workspace/async/src/notify.rs`

**Status:** Fully implemented and migrated in bpa/

**API:**

```rust
#[cfg(feature = "tokio")]
pub struct Notify(tokio::sync::Notify);

impl Notify {
    pub fn new() -> Self;
    pub fn notify_one(&self);
    pub fn notified(&self) -> impl Future<Output = ()> + '_;
}

impl Default for Notify { ... }
```

**Purpose:** Runtime-agnostic notification primitive for waking async tasks. Wraps `tokio::sync::Notify` with feature-gating for future Embassy support.

**Migrations Completed:**

- `bpa/src/rib/mod.rs` - poll_waiting_notify
- `bpa/src/storage/mod.rs` - reaper_wakeup field
- `bpa/src/storage/store.rs` - reaper_wakeup initialization
- `bpa/src/storage/channel.rs` - Shared.notify field and initialization

---

### 7. Sleep Function

**Location:** `/workspace/async/src/time.rs`

**Status:** Fully implemented and migrated in bpa/

**API:**

```rust
/// Sleeps for the specified duration.
/// - Positive durations: sleeps for the specified time
/// - Zero or negative durations: returns immediately
/// - Durations exceeding MAX: sleeps for MAX
#[cfg(feature = "tokio")]
pub async fn sleep(duration: time::Duration) {
    if !duration.is_positive() {
        return;
    }
    let std_duration: std::time::Duration = duration
        .try_into()
        .unwrap_or(std::time::Duration::MAX);
    tokio::time::sleep(std_duration).await;
}
```

**Key Design:** Accepts `time::Duration` directly (not `std::time::Duration`), matching the rest of the codebase's use of the `time` crate. Handles edge cases (negative, overflow) internally.

**Migrations Completed:**

- `bpa/src/storage/reaper.rs` - sleep in run_reaper loop

**Impact:** This was the last direct tokio dependency in bpa/. The bpa crate now has **zero direct tokio imports**.

---

## Major Architectural Improvements

### 1. Async Drop Problem - SOLVED

**Problem:** Drop handlers are synchronous, can't await, traditionally required `block_on()`

**Solution Implemented:**

```rust
// Registry owns TaskPool for all operations (cleanup + background work)
pub struct Registry {
    tasks: TaskPool,
    // ...
}

// Drop spawns cleanup onto Registry's TaskPool
impl Drop for Sink {
    fn drop(&mut self) {
        hardy_async::spawn!(self.registry.tasks, "drop_cleanup", async move {
            registry.unregister(resource).await;
        });
    }
}

// Shutdown waits for ALL tasks (including Drop-spawned)
pub async fn shutdown(&self) {
    self.tasks.shutdown().await;  // Waits for Drop tasks too
}
```

**Impact:**

- No `block_on()` in Drop handlers
- Guaranteed cleanup completion
- Runtime-agnostic (ready for Embassy)
- No deadlocks, no panics

---

### 2. Producer/Consumer Pattern - REFACTORED

**Problem:** Used `tokio::spawn()` to run producer and consumer concurrently.

**Solution:** Use `futures::join!` for concurrent execution without spawning:

```rust
futures::join!(
    // Producer
    async { producer.run(tx).await },
    // Consumer
    async {
        loop {
            select_biased! {
                item = rx.recv_async().fuse() => { /* process */ },
                _ = cancel_token.cancelled().fuse() => break,
            }
        }
    }
);
```

**Files Migrated:**

- `bpa/src/dispatcher/dispatch.rs` - `poll_waiting()`
- `bpa/src/storage/recover.rs` - `bundle_storage_recovery()`, `metadata_storage_recovery()`
- `bpa/src/storage/reaper.rs` - `refill_cache()`
- `bpa/src/storage/adu_reassembly.rs` - `poll_fragments()`

---

### 3. select! Macro - MIGRATED TO select_biased

**Problem:** `tokio::select!` is runtime-specific.

**Solution:** Use `futures::select_biased!` which is runtime-agnostic:

```rust
use futures::{select_biased, FutureExt};

select_biased! {
    _ = cancel_token.cancelled().fuse() => break,  // Highest priority
    item = rx.recv_async().fuse() => { /* process */ },
}
```

**Files Migrated (bpa/ package):**

- `dispatcher/dispatch.rs` - 1 location
- `storage/recover.rs` - 2 locations
- `storage/reaper.rs` - 2 locations
- `storage/adu_reassembly.rs` - 1 location
- `rib/mod.rs` - 1 location

**Note:** `select_biased!` uses explicit priority (first branch = highest priority), which is appropriate for our patterns where cancellation should always be checked first.

---

## Crate Structure

```
/workspace/async/
├── Cargo.toml
└── src/
    ├── lib.rs                  # Public API and documentation
    ├── task_pool.rs            # TaskPool implementation
    ├── bounded_task_pool.rs    # BoundedTaskPool implementation
    ├── cancellation_token.rs   # CancellationToken type alias
    ├── join_handle.rs          # JoinHandle type alias
    ├── notify.rs               # Notify wrapper
    ├── time.rs                 # Time utilities (sleep)
    └── spawn.rs                # spawn! macro
```

**Public API:**

```rust
pub mod bounded_task_pool;
pub mod cancellation_token;
pub mod join_handle;
pub mod notify;
pub mod task_pool;
pub mod time;

pub use bounded_task_pool::BoundedTaskPool;
pub use cancellation_token::CancellationToken;
pub use join_handle::JoinHandle;
pub use notify::Notify;

// spawn! macro available via hardy_async::spawn!(...)
// time::sleep() available via hardy_async::time::sleep(...)
```

---

## Cargo.toml (Current)

```toml
[package]
name = "hardy-async"
version = "0.1.0"
edition.workspace = true
description = "Runtime-agnostic async primitives for the Hardy DTN implementation"

[features]
default = ["tokio"]
tokio = ["dep:tokio", "dep:tokio-util"]
tracing = ["dep:tracing"]

[dependencies]
time = "0.3.46"
tokio = { version = "1.49.0", optional = true, features = ["rt", "macros", "time", "sync"] }
tokio-util = { version = "0.7.18", optional = true, features = ["rt"] }
tracing = { version = "0.1.44", optional = true }
```

---

## Migration Status by Crate

| Crate | TaskPool | spawn! | BoundedTaskPool | Notify | sleep | select_biased! | Status |
|-------|----------|--------|-----------------|--------|-------|----------------|--------|
| **bpa** | Complete | Complete | Complete | Complete | Complete | Complete | **Fully migrated (no direct tokio)** |
| **tcpclv4** | Complete | Complete | N/A | N/A | N/A | N/A | **Tokio-only** (see note 1) |
| **proto** | Complete | Complete | N/A | N/A | N/A | N/A | **Tokio-only** (see note 2) |
| **file-cla** | Complete | Complete | N/A | N/A | N/A | 2 locations pending | Partial |
| **localdisk-storage** | N/A | N/A | N/A | N/A | N/A | 1 location pending | Partial (+ WriteQueue pending) |
| **bpa-server** | Skipped | Skipped | N/A | N/A | N/A | N/A | Low priority |
| **tools** | No need | No need | N/A | N/A | N/A | N/A | Low priority |

**Note 1 (tcpclv4):** This crate is inherently dependent on tokio's networking stack (`tokio::net::TcpListener`, `tokio::net::TcpStream`, `tokio::io::AsyncRead/AsyncWrite`). Porting it to a generic async runtime is not sensible - embedded/no_std deployments would use a different CLA implementation appropriate to their network stack.

**Note 2 (proto):** This crate is inherently dependent on the tonic/tower/tokio gRPC stack. Porting it to a generic async runtime is not sensible - embedded/no_std deployments would use in-process trait implementations rather than gRPC for CLA, Service, and RoutingAgent APIs.

The TaskPool and spawn! migrations in these crates were completed for consistency, but full runtime abstraction is not a goal.

---

## Remaining Tokio Dependencies

### In bpa/ (Fully Abstracted)

**bpa/ has zero direct tokio dependencies.** All async primitives go through hardy-async.

| Primitive | Status |
|:----------|:-------|
| `tokio::sync::Notify` | Migrated to `hardy_async::Notify` |
| `tokio::time::sleep` | Migrated to `hardy_async::time::sleep` |
| `tokio::select!` | Migrated to `futures::select_biased!` |
| `tokio::task::JoinSet` | Replaced by `hardy_async::BoundedTaskPool` |
| `tokio::sync::Semaphore` | Encapsulated in `hardy_async::BoundedTaskPool` |
| `tokio::task::JoinHandle` | Migrated to `hardy_async::JoinHandle` |

### In Other Crates

| Crate | tokio::select! | Other Tokio Usage | Migration Goal |
|-------|----------------|-------------------|----------------|
| tcpclv4 | 5 locations | tokio::net, tokio::io | **None** - inherently tokio-dependent |
| proto | N/A | tonic, tower, hyper (tokio-based gRPC) | **None** - inherently tokio-dependent |
| file-cla | 2 locations (watcher.rs ×2) | Minor | Full abstraction possible |
| localdisk-storage | 1 location (storage.rs) | JoinSet, Semaphore, spawn_blocking, tokio::fs | Full abstraction via BlockingPool |

**Note:** localdisk-storage can become fully runtime-agnostic via the BlockingPool pattern. See `localdisk-storage/docs/WRITE_QUEUE_DESIGN.md`.

---

## What Remains (Future Work)

### Phase 2: Remaining Abstractions

#### 1. BlockingPool and Oneshot Channels

**Status:** Design complete, implementation pending

**Problem:** Crates like `localdisk-storage` need to perform blocking I/O (filesystem operations) without coupling to a specific async runtime's blocking thread pool (`tokio::task::spawn_blocking`).

**Solution:** A runtime-agnostic blocking I/O pool pattern:

```
┌─────────────────┐
│  Async API      │
│  save().await   │
└────────┬────────┘
         │ submit work
         ▼
┌─────────────────┐     ┌──────────────────────┐
│  flume channel  │────▶│  Dedicated I/O       │
│  (work queue)   │     │  Thread(s)           │
└─────────────────┘     │  - std::thread       │
         ▲              │  - std::fs           │
         │              │  - Batching support  │
         │              └──────────┬───────────┘
┌────────┴────────┐                │
│  oneshot recv   │◀───────────────┘
│  (completion)   │     result
└─────────────────┘
```

**Components:**

```rust
// hardy-async/src/oneshot.rs
// Runtime-agnostic oneshot channel for completion signaling

#[cfg(feature = "tokio")]
pub use tokio::sync::oneshot::{channel, Sender, Receiver, error::RecvError};

#[cfg(feature = "flume-oneshot")]
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = flume::bounded(1);
    (Sender(tx), Receiver(rx))
}
```

```rust
// hardy-async/src/blocking_pool.rs
// Dedicated thread pool for blocking operations

pub struct BlockingPool {
    threads: Vec<std::thread::JoinHandle<()>>,
    tx: flume::Sender<BoxedWork>,
}

impl BlockingPool {
    pub fn new(thread_count: usize) -> Self;

    /// Submit blocking work, returns oneshot receiver for result
    pub fn submit<F, T>(&self, f: F) -> oneshot::Receiver<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static;

    pub fn shutdown(self);
}

impl Default for BlockingPool {
    fn default() -> Self {
        Self::new(std::thread::available_parallelism().map(Into::into).unwrap_or(1))
    }
}
```

**Key Properties:**

- **Runtime-agnostic:** Uses `std::thread`, `flume`, and `std::fs` - no async runtime dependency
- **Async API surface:** Callers await the oneshot receiver
- **Batching support:** Pool implementations can batch work for efficiency (see localdisk-storage WriteQueue)
- **Explicit control:** Thread count not tied to runtime's internal pool

**Use Cases:**

| Crate | Operation | Benefit |
|-------|-----------|---------|
| localdisk-storage | File I/O (save, load, delete, recover) | Eliminates all tokio dependencies |
| sqlite-storage | Database operations | Could share blocking pool |
| Any crate with blocking I/O | Filesystem, network, CPU-bound work | Consistent pattern |

**See also:** `localdisk-storage/docs/WRITE_QUEUE_DESIGN.md` for a specialized implementation with fsync batching.

---

#### 2. BatchQueue - Generic Batched Work Queue

**Status:** Design complete, implementation pending

**Problem:** Both `sqlite-storage` and `localdisk-storage` need batched I/O processing with nearly identical patterns:
- Accept work via channel
- Collect batches with timeout
- Process batch on dedicated thread
- Signal completion via oneshot

**Solution:** A generic `BatchQueue<T, R>` abstraction that both crates can use:

```
┌─────────────────┐
│  Async Callers  │
│  (multiple)     │
└────────┬────────┘
         │ submit(T)
         ▼
┌─────────────────┐     ┌──────────────────────────┐
│  flume channel  │────▶│  I/O Thread              │
│  (work queue)   │     │                          │
└─────────────────┘     │  loop {                  │
         ▲              │    collect_batch()       │
         │              │    processor.process()   │
         │              │    send_completions()    │
┌────────┴────────┐     │  }                       │
│  oneshot recv   │◀────└──────────────────────────┘
│  (per request)  │
└─────────────────┘
```

**API:**

```rust
// hardy-async/src/batch_queue.rs

/// A request submitted to the batch queue
pub struct BatchRequest<T, R> {
    pub data: T,
    pub completion: oneshot::Sender<R>,
}

/// Trait for processing batches of work
pub trait BatchProcessor<T, R>: Send + 'static {
    /// Process a batch of requests, returning results for each
    fn process(&mut self, batch: &mut Vec<BatchRequest<T, R>>);
}

/// Configuration for batch collection
pub struct BatchConfig {
    /// Maximum requests per batch
    pub batch_size: usize,
    /// Maximum time to wait for batch to fill
    pub batch_timeout: Duration,
    /// Channel capacity (0 = unbounded)
    pub channel_capacity: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            batch_timeout: Duration::from_millis(5),
            channel_capacity: 0, // unbounded
        }
    }
}

/// Generic batched work queue
pub struct BatchQueue<T, R> {
    tx: flume::Sender<BatchRequest<T, R>>,
    _io_thread: std::thread::JoinHandle<()>,
}

impl<T: Send + 'static, R: Send + 'static> BatchQueue<T, R> {
    /// Create a new batch queue with the given processor
    pub fn new<P: BatchProcessor<T, R>>(processor: P, config: BatchConfig) -> Self;

    /// Submit work to the queue, returns receiver for result
    pub fn submit(&self, data: T) -> oneshot::Receiver<R>;

    /// Check if queue is still accepting work
    pub fn is_open(&self) -> bool;
}

impl<T, R> Drop for BatchQueue<T, R> {
    fn drop(&mut self) {
        // Close channel, thread drains remaining work
    }
}
```

**I/O Thread Loop:**

```rust
fn batch_loop<T, R, P: BatchProcessor<T, R>>(
    rx: flume::Receiver<BatchRequest<T, R>>,
    mut processor: P,
    config: BatchConfig,
) {
    let mut batch = Vec::with_capacity(config.batch_size);

    loop {
        batch.clear();

        // Block on first request
        let Ok(first) = rx.recv() else {
            break; // Channel closed
        };
        batch.push(first);

        // Collect more until batch full or timeout
        let deadline = Instant::now() + config.batch_timeout;
        while batch.len() < config.batch_size {
            match rx.recv_deadline(deadline) {
                Ok(req) => batch.push(req),
                Err(_) => break,
            }
        }

        // Process the batch
        processor.process(&mut batch);
    }
}
```

**Usage - sqlite-storage:**

```rust
// sqlite-storage/src/insert_queue.rs

struct InsertProcessor {
    conn: rusqlite::Connection,
}

impl BatchProcessor<Bundle, Result<bool>> for InsertProcessor {
    fn process(&mut self, batch: &mut Vec<BatchRequest<Bundle, Result<bool>>>) {
        // BEGIN IMMEDIATE
        let tx = self.conn.transaction().unwrap();

        for req in batch.iter() {
            let result = insert_bundle(&tx, &req.data);
            // Store result for later
        }

        // COMMIT (single fsync for entire batch)
        tx.commit().unwrap();

        // Send completions
        for req in batch.drain(..) {
            let _ = req.completion.send(Ok(true));
        }
    }
}

// Usage
let queue: BatchQueue<Bundle, Result<bool>> = BatchQueue::new(
    InsertProcessor { conn },
    BatchConfig::default(),
);
```

**Usage - localdisk-storage:**

```rust
// localdisk-storage/src/write_queue.rs

struct WriteProcessor {
    store_root: PathBuf,
    durability: DurabilityMode,
}

impl BatchProcessor<WriteRequest, Result<PathBuf>> for WriteProcessor {
    fn process(&mut self, batch: &mut Vec<BatchRequest<WriteRequest, Result<PathBuf>>>) {
        // Group by directory
        let mut dir_groups: HashMap<PathBuf, Vec<_>> = HashMap::new();
        // ... group requests ...

        // Write files, batch fsync per directory
        for (dir, requests) in dir_groups {
            for req in requests {
                let path = write_file(&dir, &req.data);
                let _ = req.completion.send(Ok(path));
            }
            fsync_directory(&dir);
        }
    }
}

// Usage
let queue: BatchQueue<WriteRequest, Result<PathBuf>> = BatchQueue::new(
    WriteProcessor { store_root, durability },
    BatchConfig { batch_size: 50, ..Default::default() },
);
```

**Performance Benefits:**

| Crate | Without Batching | With Batching | Improvement |
|-------|------------------|---------------|-------------|
| sqlite-storage | 5ms per INSERT | 0.2ms amortized | ~25x |
| localdisk-storage | 7-15ms per write | 0.2-0.5ms amortized | 10-30x |

**Key Properties:**

- **Runtime-agnostic:** Uses `std::thread` and `flume` - no async runtime in I/O path
- **Generic:** Works with any request/response types
- **Composable:** Processors can implement domain-specific batching strategies
- **Backpressure:** Bounded channel option for flow control
- **Graceful shutdown:** Drains pending work on drop

**See also:**
- `sqlite-storage/docs/INSERT_BATCH_DESIGN.md` - Domain-specific INSERT batching
- `localdisk-storage/docs/WRITE_QUEUE_DESIGN.md` - Domain-specific write batching with fsync

---

#### 3. Signal Handling

**Status:** Not yet implemented

**Current Duplicates:**

- `bpa-server/src/main.rs` - Full implementation
- `tools/src/ping/cancel.rs` - Simplified version

**Proposed:**

```rust
pub fn listen_for_cancel(cancel_token: &CancellationToken) {
    // Listens for SIGTERM (Unix) and CTRL+C, cancels token when received
}
```

---

#### 4. select_biased! Migration in Other Crates

**Status:** Pending (excluding tcpclv4)

**Remaining locations:**

- file-cla: 2 locations (watcher.rs:68, watcher.rs:95)
- localdisk-storage: 1 location (storage.rs:160)

**Excluded:** tcpclv4 (5 locations) - This crate remains tokio-only due to inherent dependency on tokio networking. Embedded deployments would use a different CLA.

---

### Phase 3: Embassy Support

#### Cargo.toml Changes

```toml
[features]
default = ["tokio"]

# Runtime selection (mutually exclusive)
tokio = ["dep:tokio", "dep:tokio-util"]
embassy = [
    "dep:embassy-executor",
    "dep:embassy-sync",
    "dep:embassy-time",
    "dep:embassy-futures",
]

[dependencies]
# Embassy runtime (optional)
embassy-executor = { version = "0.6", optional = true }
embassy-sync = { version = "0.6", optional = true }
embassy-time = { version = "0.3", optional = true }
embassy-futures = { version = "0.1", optional = true }

# Runtime-agnostic
futures = "0.3"
```

#### Implementation Steps

1. Add Embassy feature flag and dependencies
2. Implement Embassy backends for Notify, sleep, Semaphore
3. Abstract CancellationToken (or use embassy equivalent)
4. Test on embedded target (STM32, ESP32, etc.)

---

## Runtime-Agnostic Components (Already Done)

These components work with any async runtime:

- `async-trait` - Trait definitions for storage, CLA, etc.
- `flume` - MPMC channels (used throughout)
- `bytes` - Buffer type (not Tokio-specific)
- `futures` - Used for `join!`, `select_biased!`, `FutureExt`
- `hardy-cbor` - Already `no_std` compatible
- `hardy-bpv7` - Already `no_std` compatible

---

## Key Design Decisions

### 1. Separate Crate (Not Export from BPA)

**Decision:** Created `/workspace/async/` as standalone crate

**Rationale:**

- Clean separation of concerns
- No coupling to BPA
- Can be used by any Hardy crate

---

### 2. Registry-Owned TaskPool (Not Per-Resource)

**Decision:** Single TaskPool per registry, not per CLA/Service

**Rationale:**

- Background tasks exit naturally when channels close
- No need for scoped shutdown
- Simpler architecture

---

### 3. TaskPool Pattern for Drop (Not spawn_detached)

**Decision:** Spawn onto registry's TaskPool from Drop handlers

**Rationale:**

- Guaranteed completion (via shutdown)
- Tracked cleanup (not fire-and-forget)
- More robust than detached spawning

---

### 4. BoundedTaskPool for Parallelism

**Decision:** Encapsulate Semaphore + TaskPool in BoundedTaskPool

**Rationale:**

- Hides runtime-specific Semaphore
- Simple API with `spawn().await`
- Default uses `available_parallelism()`

---

### 5. select_biased! over tokio::select

**Decision:** Use `futures::select_biased!` instead of `tokio::select!`

**Rationale:**

- Runtime-agnostic (works with Embassy)
- Explicit priority ordering prevents subtle bugs
- Our patterns (work vs cancel) don't need random fairness

---

## Statistics - What We Achieved

| Metric | Status |
|--------|--------|
| **Crates Fully Migrated** | 1 (bpa) |
| **Crates Partially Migrated** | 2 (file-cla, localdisk-storage) |
| **Crates Tokio-Only** | 2 (tcpclv4, proto - inherently tokio-dependent) |
| **Lines of Boilerplate Eliminated** | ~200+ lines |
| **TaskPool Adoptions** | 9 modules/structs |
| **spawn! Macro Adoptions** | 7 call sites |
| **BoundedTaskPool Adoptions** | 1 (dispatch.rs) |
| **Notify Migrations** | 4 locations in bpa/ |
| **sleep Migrations** | 1 location in bpa/ |
| **select_biased! Migrations** | 7 locations in bpa/ |
| **select_biased! Remaining** | 3 locations (file-cla: 2, localdisk-storage: 1) |
| **async Drop Issues Fixed** | 2 (CLA + Service registries) |
| **Untracked Tasks Fixed** | 3 (peers, proxy, registries) |
| **bpa/ Direct Tokio Deps** | **0** (fully abstracted) |

---

## Quick Reference: Migration Patterns

| Before | After |
|--------|-------|
| `tokio::spawn(f)` | `spawn!(task_pool, "name", f)` |
| `tokio::select! { ... }` | `futures::select_biased! { ... }` |
| `tokio::task::JoinSet` + `Semaphore` | `BoundedTaskPool` |
| `tokio::spawn(producer); tokio::spawn(consumer);` | `futures::join!(producer, consumer)` |
| `tokio::task::spawn_blocking(f)` | `BlockingPool::submit(f)` (future) |
| `tokio::fs::*` operations | `std::fs` via BlockingPool (future) |
| Per-operation spawn_blocking + fsync | `BatchQueue` with batched fsync (future) |
| Per-INSERT transaction | `BatchQueue` with batched transactions (future) |
| `tokio::sync::Notify` | `hardy_async::Notify` |
| `tokio::time::sleep(d)` | `hardy_async::time::sleep(d)` |

---

## Conclusion

**Phase 1 of the hardy-async implementation is complete.** The core abstractions (TaskPool, spawn! macro, BoundedTaskPool, JoinHandle, CancellationToken, Notify, sleep) are implemented and migrated.

**The bpa/ crate now has zero direct tokio dependencies.** All async primitives are accessed through hardy-async abstractions. This means bpa/ is ready for Embassy support once the abstractions have Embassy backends.

**Phase 2 is in progress.** Remaining work includes:

- **BlockingPool and oneshot channels** - Runtime-agnostic blocking I/O pattern
  - Enables localdisk-storage to become fully runtime-agnostic
  - See `localdisk-storage/docs/WRITE_QUEUE_DESIGN.md` for specialized implementation
- **BatchQueue** - Generic batched work queue for high-throughput I/O
  - Shared abstraction for sqlite-storage and localdisk-storage
  - 10-30x performance improvement through batching
  - See `sqlite-storage/docs/INSERT_BATCH_DESIGN.md` and `localdisk-storage/docs/WRITE_QUEUE_DESIGN.md`
- Migrating `select_biased!` in remaining crates (3 locations: file-cla: 2, localdisk-storage: 1)
- Signal handling abstraction

**Note:** tcpclv4 and proto are excluded from migration goals - they are inherently dependent on tokio (networking and tonic/gRPC respectively). Embedded deployments would use different CLA implementations and in-process trait implementations rather than gRPC.

**Phase 3 (Embassy support)** can begin once Phase 2 is complete. The foundation is ready - adding Embassy backends for the abstractions is straightforward.

**Key insight:** The BlockingPool and BatchQueue patterns allow crates with blocking I/O (filesystem, database) to become fully runtime-agnostic by using dedicated OS threads + channels rather than relying on runtime-specific `spawn_blocking` implementations. The BatchQueue further improves performance by amortizing expensive operations (fsync, transaction commit) across batches.
