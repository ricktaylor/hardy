# SQLite IO Queue Design

## Status

Proposal -- partially implemented (assessed 2026-07-08). The current `sqlite-storage` has the read-connection pool and the serialized writer (`src/storage.rs` `ConnectionPool`), but not the dedicated IO threads or INSERT batching described below; the Overview describes the proposed architecture, not the current code.

## Revisited 2026-07-08 — impact of the streaming pipeline

**Verdict: unlike the localdisk write-queue proposal (retired 2026-07-08; its surviving advice — the spool-and-commit engine with batched, directory-coalesced fsyncs — now lives in the [localdisk-storage design](../../localdisk-storage/docs/design.md) §Future Work), this design survives the streaming pipeline largely intact — what changes is the batching trigger and the op mix, not the architecture.**

- **The unit of work is untouched.** Streaming re-plumbs payload bytes (`BundleStorage`), but the metadata/data split is preserved and `MetadataStorage` stays row-oriented; the streaming BPA is explicitly "a stateless pipeline over durable state" passing keys and lightweight metadata views. Rows stay small and batchable, so the INSERT batching, single write thread, read pool, and the whole WAL analysis remain valid as written.
- **New coupling: the batching window chains with the data-commit window.** The late `Ingress` hook fires "after bundle fully stored", so the metadata insert must follow the bundle-data spool *commit*. If localdisk adopts the batched spool-and-commit engine, two naive independent batch windows add up (spool → data-commit window → insert window) on every ingress. Recommendation: drive metadata INSERT batches off completed data-commit batches — the set of spools fsynced in one commit batch *is* the next INSERT batch. One shared window, and crash-ordering (data durable before metadata references it) falls out for free.
- **Op-mix shift.** With the streaming pipeline persisting a status transition each time a key moves between queues, `replace()` (status updates) traffic is at least as hot as `insert()`. The §Operation Routing table sends `replace()` direct for latency; before implementing, re-measure the op mix under the streaming pipeline — batching may want to cover status updates too, or the routing decision may deserve a latency budget per status class.
- **The WAL prerequisite is satisfied in code, not in the schema.** The `PRAGMA journal_mode = WAL` in `schemas/01_setup.sql` is inert — journal mode cannot be changed inside the migration transaction, and SQLite refuses silently (the schema file is hash-locked, so the dead pragma stays). WAL is applied at connection setup in `src/storage.rs` instead, and the §WAL Mode analysis below applies to the current code.
- **Sequencing:** implement alongside the streaming storage traits, coordinated with the localdisk commit queue (shared batching window above).

## Overview

All SQLite operations run on dedicated IO threads, keeping the tokio async runtime free from blocking disk I/O. Writes go through a single thread with INSERT batching. Reads go through a pool of threads with read-only connections. WAL mode enables concurrent reads alongside writes.

### Architecture

```
                    Async Tasks
                   /           \
         insert/replace/       get/poll_expiry/
         tombstone/etc         poll_waiting/etc
              |                      |
              v                      v
    ┌──────────────────┐   ┌──────────────────┐
    │   Write Queue    │   │   Read Queue     │
    │ (flume channel)  │   │ (flume channel)  │
    └────────┬─────────┘   └────────┬─────────┘
             v                      v
    ┌──────────────────┐   ┌──────────────────┐
    │ Single IO Thread │   │  IO Thread Pool  │
    │ (serialized,     │   │  (concurrent,    │
    │  INSERT batching)│   │   read-only      │
    │                  │   │   connections)    │
    └────────┬─────────┘   └────────┬─────────┘
             │                      │
             v                      v
    ┌──────────────────────────────────────────┐
    │        SQLite Database (WAL mode)        │
    └──────────────────────────────────────────┘
```

### Operation Routing

**Write queue (single thread):**
- `insert()` -> batched (high volume, identical operations)
- `replace()` -> direct (status updates, low latency needed)
- `tombstone()` -> direct (immediate cleanup)
- `confirm_exists()` -> direct (startup only, contains DELETE + SELECT)
- `remove_unconfirmed()` -> direct (startup only, transactional)
- `poll_waiting()` -> direct (writes to waiting_queue table)
- `start_recovery()` -> direct (startup only)
- `reset_peer_queue()` -> direct (control operation)

**Read pool (concurrent threads):**
- `get()` -> read pool
- `poll_expiry()` -> read pool
- `poll_pending()` -> read pool
- `poll_adu_fragments()` -> read pool

Serialization is inherent in the single write thread, eliminating the need for a `tokio::sync::Mutex` write lock.

## WAL Mode

This architecture requires WAL mode.

In classic mode, the write thread acquires an EXCLUSIVE lock during commit + fsync. This blocks every read pool thread for the duration of the sync -- and with INSERT batching, write transactions are larger and held longer, making the blocking worse. A 100-insert batch commit could stall the entire read pool for several milliseconds.

In WAL mode, readers see a consistent snapshot from when their read started. The write thread can commit a 100-insert batch while all read pool threads continue serving reads unblocked. The only serialization is writer-to-writer, which is inherent in having a single write thread and therefore costs nothing.

The IO queue architecture amplifies WAL's strengths and avoids its weakness:
- Single write thread = no writer-writer contention (WAL's one limitation is irrelevant)
- Multiple read threads = full concurrent read throughput (WAL's main benefit is fully exploited)

**Checkpointing:**

WAL auto-checkpoint (default: 1000 pages) runs inside the connection that triggers it. In this model that is always the write thread, which keeps checkpoint I/O off the async runtime. If checkpoint latency during an INSERT batch becomes a concern, a `PRAGMA wal_checkpoint(PASSIVE)` can be run periodically on one of the read connections -- PASSIVE checkpoints only what is possible without blocking the writer.

## Implementation Details

### Data Structures

```rust
pub struct Storage {
    write_queue: WriteQueue,
    read_pool: ReadPool,
}

// Write queue
struct WriteQueue {
    tx: flume::Sender<WriteRequest>,
    _io_thread: std::thread::JoinHandle<()>,
}

enum WriteRequest {
    Insert {
        data: InsertData,
        completion: oneshot::Sender<storage::Result<bool>>,
    },
    Direct {
        op: Box<dyn FnOnce(&mut rusqlite::Connection) -> storage::Result<Box<dyn Any + Send>> + Send>,
        completion: oneshot::Sender<storage::Result<Box<dyn Any + Send>>>,
    },
}

struct InsertData {
    id: Vec<u8>,
    bundle: Vec<u8>,
    expiry: time::OffsetDateTime,
    received_at: time::OffsetDateTime,
    status_code: i64,
    status_param1: Option<i64>,
    status_param2: Option<i64>,
    status_param3: Option<String>,
}

// Read pool
struct ReadPool {
    tx: flume::Sender<ReadRequest>,
    _io_threads: Vec<std::thread::JoinHandle<()>>,
}

struct ReadRequest {
    op: Box<dyn FnOnce(&mut rusqlite::Connection) -> storage::Result<Box<dyn Any + Send>> + Send>,
    completion: oneshot::Sender<storage::Result<Box<dyn Any + Send>>>,
}

struct WriteQueueConfig {
    batch_size: usize,         // Max inserts per batch (default: 100)
    batch_timeout_ms: u64,     // Max wait to accumulate batch (default: 5ms)
    optimize_interval: usize,  // PRAGMA optimize every N writes (default: 1000)
}

struct ReadPoolConfig {
    pool_size: usize,          // Number of reader threads (default: min(num_cpus, 4))
}
```

### Write Queue

```rust
impl WriteQueue {
    fn new(path: PathBuf, config: WriteQueueConfig) -> Self {
        let (tx, rx) = flume::unbounded();

        let io_thread = std::thread::spawn(move || {
            let mut conn = open_connection(&path, false);
            write_loop(rx, &mut conn, config);
        });

        Self {
            tx,
            _io_thread: io_thread,
        }
    }

    fn submit(&self, req: WriteRequest) {
        self.tx.send(req).trace_expect("Write queue shut down");
    }
}
```

### Write Thread Loop

```rust
fn write_loop(
    rx: flume::Receiver<WriteRequest>,
    conn: &mut rusqlite::Connection,
    config: WriteQueueConfig,
) {
    let mut insert_batch = Vec::with_capacity(config.batch_size);
    let mut write_count: usize = 0;

    loop {
        insert_batch.clear();

        // Block on first request
        let Ok(first) = rx.recv() else { break };

        match first {
            WriteRequest::Insert { .. } => {
                insert_batch.push(first);

                // Collect more inserts (up to batch_size or timeout)
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(config.batch_timeout_ms);

                while insert_batch.len() < config.batch_size {
                    match rx.recv_deadline(deadline) {
                        Ok(req @ WriteRequest::Insert { .. }) => insert_batch.push(req),
                        Ok(other) => {
                            // Non-insert arrived: flush insert batch first, then handle it
                            process_insert_batch(&mut insert_batch, conn);
                            process_direct_write(other, conn);
                            break;
                        }
                        Err(_) => break,
                    }
                }

                if !insert_batch.is_empty() {
                    process_insert_batch(&mut insert_batch, conn);
                }
            }
            WriteRequest::Direct { .. } => {
                process_direct_write(first, conn);
            }
        }

        write_count += 1;
        if write_count % config.optimize_interval == 0 {
            _ = conn.execute_batch("PRAGMA optimize;");
        }
    }
}
```

### Batch Processing

```rust
fn process_insert_batch(
    batch: &mut Vec<WriteRequest>,
    conn: &mut rusqlite::Connection,
) {
    let tx = match conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(e) => {
            let err = storage::Error::from(e);
            for req in batch.drain(..) {
                if let WriteRequest::Insert { completion, .. } = req {
                    _ = completion.send(Err(err.clone()));
                }
            }
            return;
        }
    };

    // Prepare statement once for entire batch
    let mut stmt = tx.prepare_cached(
        "INSERT OR IGNORE INTO bundles
         (bundle_id, bundle, expiry, received_at, status_code, status_param1, status_param2, status_param3)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
    ).trace_expect("Failed to prepare insert statement");

    for req in batch.drain(..) {
        if let WriteRequest::Insert { data, completion } = req {
            let result = stmt.execute((
                &data.id, &data.bundle, data.expiry, data.received_at,
                data.status_code, data.status_param1, data.status_param2, &data.status_param3,
            )).map(|c| c == 1).map_err(Into::into);
            _ = completion.send(result);
        }
    }

    drop(stmt);

    if let Err(e) = tx.commit() {
        error!("Failed to commit insert batch: {e}");
    }
}

fn process_direct_write(req: WriteRequest, conn: &mut rusqlite::Connection) {
    if let WriteRequest::Direct { op, completion } = req {
        _ = completion.send(op(conn));
    }
}
```

### Read Pool

```rust
impl ReadPool {
    fn new(path: PathBuf, config: ReadPoolConfig) -> Self {
        let (tx, rx) = flume::unbounded();

        let io_threads = (0..config.pool_size)
            .map(|_| {
                let rx = rx.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut conn = open_connection(&path, true);
                    read_loop(rx, &mut conn);
                })
            })
            .collect();

        Self {
            tx,
            _io_threads: io_threads,
        }
    }

    fn submit(&self, req: ReadRequest) {
        self.tx.send(req).trace_expect("Read pool shut down");
    }
}

fn read_loop(
    rx: flume::Receiver<ReadRequest>,
    conn: &mut rusqlite::Connection,
) {
    while let Ok(ReadRequest { op, completion }) = rx.recv() {
        _ = completion.send(op(conn));
    }
}
```

### Connection Factory

```rust
fn open_connection(path: &PathBuf, read_only: bool) -> rusqlite::Connection {
    let flags = if read_only {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
    };

    let conn = rusqlite::Connection::open_with_flags(path, flags)
        .trace_expect("Failed to open connection");

    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA optimize = 0x10002;",
    ).trace_expect("Failed to configure connection");

    rusqlite::vtab::array::load_module(&conn)
        .trace_expect("Failed to load array module");

    conn
}
```

### Integration with MetadataStorage Trait

```rust
#[async_trait]
impl storage::MetadataStorage for Storage {
    async fn insert(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<bool> {
        let (tx, rx) = oneshot::channel();
        let data = InsertData::from(bundle)?;
        self.write_queue.submit(WriteRequest::Insert { data, completion: tx });
        rx.await.map_err(|_| /* queue closed */)?
    }

    async fn replace(&self, bundle: &hardy_bpa::bundle::Bundle) -> storage::Result<()> {
        // Direct write via queue (not batched)
        let (tx, rx) = oneshot::channel();
        self.write_queue.submit(WriteRequest::Direct {
            op: Box::new(move |conn| { /* UPDATE ... */ }),
            completion: tx,
        });
        rx.await.map_err(|_| /* queue closed */)?
    }

    async fn get(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<Option<Bundle>> {
        // Read via pool
        let (tx, rx) = oneshot::channel();
        self.read_pool.submit(ReadRequest {
            op: Box::new(move |conn| { /* SELECT ... */ }),
            completion: tx,
        });
        rx.await.map_err(|_| /* pool closed */)?
    }
}
```

## SQLite Optimizations from Batching

When executing 100 identical INSERTs in a single transaction, SQLite can:

1. **Reuse query plan** - Parse once, execute 100 times
2. **Batch index updates** - Update indexes in larger chunks
3. **Amortize B-tree rebalancing** - Fewer tree restructures
4. **Single fsync** - One sync for entire batch (even with WAL)
5. **Better page cache utilization** - Hot pages stay in cache
6. **Reduced WAL overhead** - One WAL entry per batch, not per insert

## Performance Expectations

### Write Throughput

```
1000 inserts batched:
- 10 batches of 100 inserts each
- Per batch: ~20ms (one transaction + fsync)
- Total: 10 * 20ms = 200ms
- Tokio worker threads: never blocked

Amortized per insert: 0.2ms (vs ~5ms unbatched)
```

### Read Concurrency

```
10 concurrent reads:
- Dispatched to pool threads via flume work-stealing
- SQLite WAL allows true concurrent reads
- Tokio workers: never blocked, immediately available for other async work
```

### Full Pipeline (BPA ingestion)

```
With IO queues (both localdisk and SQLite):
- Localdisk save: 10 batches * 10ms = 100ms
- SQLite insert:  10 batches * 20ms = 200ms
- Total: 300ms for 1000 bundles
```

## Thundering Herd Considerations

**Two thundering herd events in full pipeline:**

1. **Localdisk write queue completes** -> 100 tasks wake, call insert()
2. **SQLite insert queue completes** -> 100 tasks wake, return to caller

**Why this is acceptable:**

- Not true contention (tasks receive results and proceed independently)
- Tokio work-stealing scheduler distributes wakes across threads
- No lock competition or retry loops
- Batch size limits maximum simultaneous wakes (100, not 10,000)

This is pipeline flow, not a performance problem.

## Configuration Recommendations

### High Throughput (Cloud Deployment)

```rust
WriteQueueConfig {
    batch_size: 100,
    batch_timeout_ms: 5,
    optimize_interval: 1000,
}
ReadPoolConfig {
    pool_size: 4,
}
```

### Low Latency (Edge Deployment)

```rust
WriteQueueConfig {
    batch_size: 10,
    batch_timeout_ms: 1,
    optimize_interval: 5000,
}
ReadPoolConfig {
    pool_size: 2,
}
```

### Testing/Development

```rust
WriteQueueConfig {
    batch_size: 1,
    batch_timeout_ms: 0,
    optimize_interval: 100,
}
ReadPoolConfig {
    pool_size: 1,
}
```

## Error Handling

### Transaction Begin Errors

```rust
let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate);
// If this fails (e.g., SQLITE_BUSY), fail all requests in batch
// Callers receive individual errors via their oneshot channels
```

### Individual INSERT Errors

```rust
for req in batch {
    let result = stmt.execute((...));
    _ = req.completion.send(result);
}
// One failed insert doesn't fail the whole batch
// COMMIT still proceeds for successful inserts
```

### Commit Errors

```rust
if let Err(e) = tx.commit() {
    error!("Failed to commit insert batch: {e}");
    // Individual results already sent -- this is a consistency issue
    // Logged for investigation
}
```

### Queue Shutdown

If an async task's oneshot receiver gets a `RecvError`, the queue has shut down. This surfaces as a storage error to the caller.

## Shutdown Behavior

```rust
impl Drop for WriteQueue {
    fn drop(&mut self) {
        // 1. Sender dropped - no new requests accepted
        // 2. IO thread drains remaining requests in channel
        // 3. IO thread exits when channel is empty and closed
    }
}

impl Drop for ReadPool {
    fn drop(&mut self) {
        // 1. Sender dropped - no new requests accepted
        // 2. Each reader thread drains its current operation
        // 3. Threads exit when channel is empty and closed
    }
}
```

Pending requests are processed. In-flight operations complete. No work is lost.

## Open Questions

1. **Should batch_size be dynamic?** Adjust based on observed latency/throughput?
2. **Should PRAGMA optimize run on graceful shutdown?** Cleanup before exit?
3. **Metrics integration?** Expose batch size, queue depth, throughput via metrics crate?
4. **Read pool sizing:** Should pool size be configurable or auto-detected from CPU count?
5. **Read-only connections:** Verify all read-path operations work correctly with `SQLITE_OPEN_READ_ONLY` connections (rarray, prepare_cached, etc.)

## References

- SQLite transaction performance: https://www.sqlite.org/faq.html#q19
- WAL mode: https://www.sqlite.org/wal.html
- WAL concurrent readers: https://www.sqlite.org/wal.html#concurrency
- PRAGMA optimize: https://www.sqlite.org/pragma.html#pragma_optimize
