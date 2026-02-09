# Hardy-Async Crate Status

**Last Updated:** 2026-02-09

## Overview

The `hardy-async` crate provides runtime-agnostic async primitives for the Hardy DTN implementation, enabling future support for both Tokio (cloud/server) and Embassy (embedded/no_std) runtimes.

**Status:** Phase 1 Complete, Phase 2 In Progress (std feature added, spinlock optimizations complete, sync module complete)

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
    ├── spawn.rs                # spawn! macro
    └── sync/                   # Synchronization primitives
        ├── mod.rs              # Mutex, RwLock (std wrappers with trace_expect)
        └── spin.rs             # spin::Mutex, spin::RwLock wrappers
```

**Public API:**

```rust
pub mod bounded_task_pool;
pub mod cancellation_token;
pub mod join_handle;
pub mod notify;
pub mod sync;
pub mod task_pool;
pub mod time;

pub use bounded_task_pool::BoundedTaskPool;
pub use cancellation_token::CancellationToken;
pub use join_handle::JoinHandle;
pub use notify::Notify;

// spawn! macro available via hardy_async::spawn!(...)
// time::sleep() available via hardy_async::time::sleep(...)
// sync::Mutex, sync::RwLock available via hardy_async::sync::{Mutex, RwLock}
// sync::spin::Mutex, sync::spin::RwLock available via hardy_async::sync::spin::{Mutex, RwLock}
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
std = ["time/std"]
tokio = ["std", "dep:tokio", "dep:tokio-util"]  # tokio implies std
tracing = ["dep:tracing"]

[dependencies]
time = { version = "0.3", default-features = false }
async-trait = "0.1"
tokio = { version = "1.49.0", optional = true, features = ["rt", "macros", "time", "sync"] }
tokio-util = { version = "0.7.18", optional = true, features = ["rt"] }
tracing = { version = "0.1.44", optional = true }
spin = "0.9.8"           # Spinlock primitives (no_std compatible)
trace-err = "0.1.5"      # Error tracing for unified poison handling
```

**Feature Chain:**
- `tokio` implies `std` (tokio requires std)
- `std` enables `time/std`
- Future `embassy` feature will NOT imply `std`

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

### Phase 2.5a: Spinlock Optimizations (Completed)

**Status:** ✅ Complete

**Problem:** The `bpa` crate uses OS-level `std::sync::{Mutex, RwLock}` for all synchronization, even for O(1) operations where spinlocks would be more efficient.

**Solution:** Audit all Mutex/RwLock usages and convert appropriate ones to `spin::Mutex` or `spin::RwLock`, or lock-free atomics where possible.

#### Criteria for Spinlock Suitability

A lock is suitable for spinlock conversion if ALL of these are true:
1. **O(1) operations only** - No iteration while holding lock
2. **No blocking** - No I/O, RNG syscalls, or sleeping
3. **Short critical sections** - Just HashMap lookups/inserts, state checks
4. **Lock released before async** - No holding lock across await points

#### Changes Implemented

**Converted to `spin::Mutex`:**

| Location | Field | Rationale |
|----------|-------|-----------|
| `storage/store.rs` | `bundle_cache` | O(1) LRU peek/put/pop |
| `services/registry.rs` | `services` | O(1) HashMap ops, RNG moved outside lock |
| `cla/registry.rs` | `Cla::peers` | O(1) HashMap ops |
| `cla/registry.rs` | `Registry::clas` | O(1) HashMap ops |

**Converted to `spin::RwLock`:**

| Location | Field | Rationale |
|----------|-------|-----------|
| `cla/peers.rs` | `PeerTable::inner` | O(1) HashMap, read-heavy forwarding path |

**Converted to Lock-Free Atomics:**

| Location | Field | Rationale |
|----------|-------|-----------|
| `storage/channel.rs` | `Shared::state` | `#[repr(usize)]` enum with `AtomicUsize` CAS |

**Kept as `std::sync::Mutex`:**

| Location | Field | Rationale |
|----------|-------|-----------|
| `storage/mod.rs` | `reaper_cache` | While loop pops multiple entries on mass expiry |
| `storage/metadata_mem.rs` | `entries` | O(n) iteration in poll_* methods |
| `storage/bundle_mem.rs` | `inner` | O(n) iteration in recover() |

**Kept as `std::sync::RwLock`:**

| Location | Field | Rationale |
|----------|-------|-----------|
| `filters/registry.rs` | `inner` | O(n) iteration in prepare/add/remove |
| `keys/registry.rs` | `providers` | O(n) `.values().cloned().collect()` |
| `rib/mod.rs` | `inner` | Complex recursive lookup/iteration |

#### Key Refactorings

**1. Lock-Free Channel State Machine (`storage/channel.rs`):**

Replaced `spin::Mutex<ChannelState>` with lock-free atomics:

```rust
#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChannelState {
    Open = 0,
    Draining = 1,
    Congested = 2,
    Closing = 3,
}

struct Shared {
    state: AtomicUsize,  // Lock-free state machine
    // ...
}

impl Shared {
    fn compare_exchange_state(&self, current: ChannelState, new: ChannelState) -> Result<...> {
        self.state.compare_exchange(
            current.as_usize(),
            new.as_usize(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
    }
}
```

**2. RNG Outside Lock (`services/registry.rs`):**

The original code called RNG inside the lock, which could involve syscalls (getrandom). Refactored to generate candidates outside:

```rust
// BEFORE: RNG inside lock (potential syscall while spinning)
let mut services = self.services.lock();
loop {
    let candidate = rng.random_range(...);  // RNG here!
    if !services.contains_key(&candidate) { break; }
}

// AFTER: RNG outside lock, only O(1) ops inside
loop {
    let candidate = rand::rng().random_range(...);  // RNG outside
    let mut services = self.services.lock();
    if !services.contains_key(&candidate) {
        services.insert(candidate, service);
        break;
    }
    // Lock dropped, try new random
}
```

**3. Decoupled Nested Lock Acquisition (`cla/registry.rs`):**

Fixed potential spinlock nesting in `add_peer()`:

```rust
// BEFORE: Nested spinlock acquisition
let mut peers = cla.peers.lock();  // First lock
let peer_id = self.peers.insert(peer);  // Takes second lock while holding first!
peers.entry(node_id).insert(peer_id);

// AFTER: Sequential acquisition
let peer_id = self.peers.insert(peer);  // Get ID first
let inserted = {
    let mut peers = cla.peers.lock();  // Separate lock scope
    // Insert or detect collision
};
if !inserted {
    self.peers.remove(peer_id).await;  // Cleanup on collision
}
```

#### Summary

- **6 locks converted** to spinlocks or lock-free atomics
- **6 locks kept** as OS primitives (with documented rationale)
- **All spinlock usages verified** for no nested acquisition, no blocking ops

---

### Phase 2.5b: Synchronization Primitives (sync module)

**Status:** ✅ Complete

**Problem:** The `bpa` crate uses `std::sync::{Mutex, RwLock}` which return `LockResult` requiring explicit error handling. Additionally, direct `spin::` usage couples bpa to a specific spinlock implementation.

**Solution:** Add a `sync` module to hardy-async that provides:
1. **Spinlock wrappers** (`sync::spin::Mutex`, `sync::spin::RwLock`) - For O(1) operations on hot paths
2. **Std wrappers** (`sync::Mutex`, `sync::RwLock`) - For general use, with `trace_expect()` to handle poison errors

#### Implemented sync Module

**Location:** `/workspace/async/src/sync/`

```
sync/
├── mod.rs    # Mutex, RwLock (std wrappers with trace_expect)
└── spin.rs   # spin::Mutex, spin::RwLock wrappers
```

#### sync::spin - Spinlock Wrappers (O(1) operations)

```rust
use hardy_async::sync::spin::{Mutex, RwLock};

// For O(1) operations on hot paths
let cache: Mutex<HashMap<K, V>> = Mutex::new(HashMap::new());
cache.lock().insert(key, value);  // Returns guard directly

let table: RwLock<HashMap<K, V>> = RwLock::new(HashMap::new());
let value = table.read().get(&key);  // Returns guard directly
```

**When to use:**
- All operations are O(1) (HashMap lookup/insert)
- No blocking, I/O, or syscalls while holding lock
- Lock is released before any async operations
- No nested lock acquisition

**Platform implementations:**
- **std**: Wraps `spin::Mutex` / `spin::RwLock`
- **embassy** (future): Wraps `embassy_sync::mutex::Mutex<CriticalSectionRawMutex, T>`

#### sync::Mutex / sync::RwLock - Std Wrappers (General Use)

```rust
use hardy_async::sync::{Mutex, RwLock};

// For O(n) operations, iteration, or blocking I/O
let entries: Mutex<Vec<T>> = Mutex::new(Vec::new());
for item in entries.lock().iter() {  // Returns guard directly (no .unwrap()!)
    // O(n) iteration is fine
}

let providers: RwLock<HashMap<K, V>> = RwLock::new(HashMap::new());
let values: Vec<_> = providers.read().values().cloned().collect();
```

**Key feature:** Uses `trace_expect()` internally to handle poison errors, providing:
- **Unified interface** - Same API as spinlocks (returns guard directly)
- **Embassy compatibility** - Embassy has no poison concept
- **Automatic tracing** - Poison panics are logged before panic

**Implementation:**

```rust
#[cfg(feature = "std")]
pub struct Mutex<T>(std::sync::Mutex<T>);

impl<T> Mutex<T> {
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().trace_expect("Mutex poisoned")
    }
}
```

#### Migration Completed in bpa

**Converted to `sync::spin::Mutex`:**

| Location | Field |
|----------|-------|
| `storage/mod.rs` | `Store::bundle_cache` |
| `services/registry.rs` | `Registry::services` |
| `cla/registry.rs` | `Cla::peers` |
| `cla/registry.rs` | `Registry::clas` |

**Converted to `sync::spin::RwLock`:**

| Location | Field |
|----------|-------|
| `cla/peers.rs` | `PeerTable::inner` |

**Converted to `sync::Mutex`:**

| Location | Field |
|----------|-------|
| `storage/mod.rs` | `Store::reaper_cache` |
| `storage/metadata_mem.rs` | `Storage::entries` |
| `storage/bundle_mem.rs` | `Storage::inner` |

**Converted to `sync::RwLock`:**

| Location | Field |
|----------|-------|
| `filters/registry.rs` | `Registry::inner` |
| `rib/mod.rs` | `Rib::inner` |
| `keys/registry.rs` | `Registry::providers` |

**Dependencies updated:**
- Removed `spin = "0.9.8"` from bpa/Cargo.toml
- All spinlock usage now goes through hardy-async

#### Benefits Achieved

| Aspect | Before | After |
|--------|--------|-------|
| Call site code | `.lock().trace_expect("...")` | `.lock()` |
| Poison handling | Manual at each call site | Centralized in wrapper |
| Spinlock dependency | Direct in bpa | Centralized in hardy-async |
| Embassy preparation | Manual migration needed | Feature-flag ready |

#### Embassy Considerations (Future)

For Embassy (single-executor, often single-core):

| std Type | Embassy Equivalent | Notes |
|----------|-------------------|-------|
| `sync::spin::Mutex<T>` | `Mutex<CriticalSectionRawMutex, T>` | Disables interrupts briefly |
| `sync::spin::RwLock<T>` | `Mutex<CriticalSectionRawMutex, T>` | No RwLock in Embassy (single-core) |
| `sync::Mutex<T>` | `Mutex<NoopRawMutex, T>` | Zero overhead for single-executor |
| `sync::RwLock<T>` | `Mutex<NoopRawMutex, T>` | No RwLock in Embassy |

**Key Insight:** On single-core Embassy, `RwLock` degrades to `Mutex`. The read/write distinction only matters on multi-core, which Embassy doesn't typically target.

---

### Phase 2.6: Channel Abstraction

**Status:** Design complete, implementation pending

**Problem:** The `bpa` crate uses `flume` channels extensively for inter-task communication. However, flume depends on `fastrand` → `getrandom` → `libc`, which means it cannot work on bare-metal no_std targets.

**Current flume usage in bpa:**

| File | Usage |
|------|-------|
| `storage/mod.rs` | `Sender<T>` type alias |
| `storage/channel.rs` | `flume::bounded`, `Sender`, `Receiver`, `TrySendError` |
| `storage/recover.rs` | `flume::bounded` for recovery |
| `storage/reaper.rs` | `flume::bounded` for bundle cache |
| `storage/adu_reassembly.rs` | `flume::bounded` for fragments |
| `dispatcher/dispatch.rs` | `flume::bounded` for polling |

**Solution:** Abstract channels through hardy-async with feature-gated implementations.

#### flume vs embassy-sync Channels

| Aspect | flume | embassy-sync::channel |
|--------|-------|----------------------|
| **no_std** | No (requires getrandom/libc) | Yes |
| **Allocation** | Dynamic (heap) | Static buffer (compile-time capacity) |
| **MPMC** | Yes | Yes |
| **Async** | Yes (recv_async, send_async) | Yes |
| **Blocking** | Yes (recv, send) | No (async only) |
| **Backpressure** | Bounded channels | Fixed capacity |

#### Proposed API

**Location:** `/workspace/async/src/channel.rs`

```rust
//! Bounded MPMC channels with platform-appropriate implementations.
//!
//! - For `std`/`tokio`: Uses `flume` channels
//! - For `embassy`: Uses `embassy-sync::channel::Channel`

#[cfg(feature = "std")]
pub use flume::{bounded, Sender, Receiver, TrySendError, SendError, RecvError};

#[cfg(feature = "embassy")]
pub mod channel {
    use embassy_sync::channel::Channel;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

    // Embassy channels require static allocation with fixed capacity
    // This is a different API - callers need to provide static storage

    pub type StaticChannel<T, const N: usize> = Channel<CriticalSectionRawMutex, T, N>;

    // Sender/Receiver are obtained from Channel::sender()/Channel::receiver()
}
```

#### Design Considerations

**Static vs Dynamic Allocation:**

Embassy channels require static allocation with compile-time capacity. This is fundamentally different from flume's dynamic allocation. Two approaches:

1. **Wrapper with const generic:** For Embassy, channel capacity must be known at compile time
   ```rust
   // Embassy version
   static CHANNEL: StaticChannel<Bundle, 16> = Channel::new();
   let (tx, rx) = (CHANNEL.sender(), CHANNEL.receiver());
   ```

2. **Feature-gated API:** Different APIs for std vs no_std
   - std: `bounded(capacity)` returns `(Sender, Receiver)`
   - embassy: Caller provides static storage, gets sender/receiver from it

**Recommendation:** For Phase 1, keep flume for std builds. Embassy channel integration will require refactoring call sites to use static channel allocation, which is a larger change best done when adding full Embassy support.

#### Migration Path

1. **Phase 2.6a:** Create `hardy_async::channel` module with flume re-exports (transparent migration)
2. **Phase 2.6b:** Update bpa to use `hardy_async::channel` instead of direct flume imports
3. **Phase 3:** Add Embassy channel implementation (requires static allocation refactoring)

#### Actions

1. Add `channel.rs` module to hardy-async
2. Re-export flume types under `std` feature
3. Update bpa imports to use `hardy_async::channel`
4. Document Embassy's static allocation requirements for future migration

---

### Phase 3: Embassy Support

#### Cargo.toml Changes

```toml
[features]
default = ["tokio"]
std = ["time/std"]

# Runtime selection (mutually exclusive)
tokio = ["std", "dep:tokio", "dep:tokio-util"]
embassy = [
    "dep:embassy-executor",
    "dep:embassy-sync",
    "dep:embassy-time",
    "dep:embassy-futures",
]
# Note: embassy does NOT imply std

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
3. Add sync module with embassy-sync Mutex (platform-configured RawMutex)
4. Abstract CancellationToken (or use embassy equivalent)
5. Test on embedded target (STM32, ESP32, etc.)

---

## Runtime-Agnostic Components (Already Done)

These components work with any async runtime:

- `async-trait` - Trait definitions for storage, CLA, etc.
- `bytes` - Buffer type (not Tokio-specific)
- `futures` - Used for `join!`, `select_biased!`, `FutureExt`
- `hardy-cbor` - Already `no_std` compatible
- `hardy-bpv7` - Already `no_std` compatible

### Flume: std-only (not true no_std)

**Dependency chain:** `flume` → `fastrand` → `getrandom 0.2` → `libc`

While flume uses no_std-compatible primitives internally (spin, futures-core), the `fastrand` dependency requires `getrandom` which needs platform-specific entropy sources (libc on Unix). This means:

- **Works:** Linux, macOS, Windows, WASM (with wasm-bindgen)
- **Does NOT work:** Bare-metal embedded (no libc/OS)

For true no_std (Embassy), channel abstraction is needed - see Phase 2.6 below.

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
| **Lines of Boilerplate Eliminated** | ~250+ lines |
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
| **bpa/ Direct spin Deps** | **0** (via hardy_async::sync) |
| **sync::spin Migrations** | 5 (4 Mutex, 1 RwLock) |
| **sync::Mutex Migrations** | 3 locations |
| **sync::RwLock Migrations** | 3 locations |
| **Lock-Free Atomic Conversions** | 1 (channel state machine) |
| **trace_expect() Calls Removed** | 31 (centralized in sync wrappers) |
| **RNG-Outside-Lock Refactorings** | 1 (services/registry.rs) |
| **Nested Lock Fixes** | 1 (cla/registry.rs add_peer) |

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
| `spin::Mutex` (O(1) ops) | `hardy_async::sync::spin::Mutex` ✅ |
| `spin::RwLock` (O(1) ops, read-heavy) | `hardy_async::sync::spin::RwLock` ✅ |
| `Mutex<enum>` state machine | `AtomicUsize` with `#[repr(usize)]` enum ✅ |
| RNG inside lock | Generate candidate outside, check+insert inside ✅ |
| `std::sync::Mutex` (O(n) iteration) | `hardy_async::sync::Mutex` ✅ |
| `std::sync::RwLock` (O(n) iteration) | `hardy_async::sync::RwLock` ✅ |
| `.lock().trace_expect("...")` | `.lock()` (trace_expect internal) ✅ |
| `.write().trace_expect("...")` | `.write()` (trace_expect internal) ✅ |
| `.read().trace_expect("...")` | `.read()` (trace_expect internal) ✅ |

---

## Conclusion

**Phase 1 of the hardy-async implementation is complete.** The core abstractions (TaskPool, spawn! macro, BoundedTaskPool, JoinHandle, CancellationToken, Notify, sleep) are implemented and migrated.

**The bpa/ crate now has zero direct tokio or spin dependencies.** All async primitives and synchronization are accessed through hardy-async abstractions. This means bpa/ is ready for Embassy support once the abstractions have Embassy backends.

**Phase 2 is in progress.** Completed work includes:

- **Spinlock optimizations (Phase 2.5a)** - ✅ Complete
  - Converted 5 locks to spinlocks for O(1) operations
  - Converted 1 state machine to lock-free atomics (`AtomicUsize` with `#[repr(usize)]` enum)
  - Refactored 1 registry to move RNG outside lock
  - Fixed 1 nested lock acquisition issue
  - Documented criteria for spinlock suitability

- **Synchronization primitives (Phase 2.5b)** - ✅ Complete
  - Created `sync` module with spinlock and std wrappers
  - `sync::spin::Mutex` / `sync::spin::RwLock` for O(1) operations
  - `sync::Mutex` / `sync::RwLock` for general use with `trace_expect()` poison handling
  - Migrated all bpa locks to use hardy-async sync module
  - Removed direct `spin` dependency from bpa
  - Eliminated 31 explicit `.trace_expect()` calls at lock sites

Remaining work includes:

- **Channel abstraction** - Platform-appropriate MPMC channels
  - flume requires `getrandom` → `libc`, so it's std-only (not true no_std)
  - For Embassy: `embassy-sync::channel::Channel` with static allocation
  - Different allocation model (static vs dynamic) requires careful API design
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
