# Storage Subsystem Design

This document describes the storage subsystem in the BPA, covering the dual storage model, caching, expiration monitoring, and crash recovery.

## Related Documents

- **[Bundle State Machine Design](bundle_state_machine_design.md)**: Bundle status values stored in metadata
- **[Filter Subsystem Design](filter_subsystem_design.md)**: Filter checkpoint persistence
- **[Policy Subsystem Design](policy_subsystem_design.md)**: Hybrid channels for queue management
- **[Routing Design](routing_design.md)**: Route changes trigger `reset_peer_queue()`

## Overview

The storage subsystem provides persistent and cached storage for bundles, coordinating between separate data and metadata backends:

| Component | Purpose |
|-----------|---------|
| **Store** | Central coordinator for all storage operations |
| **BundleStorage** | Binary bundle data persistence (blobs) |
| **MetadataStorage** | Bundle state and lifecycle tracking |
| **LRU Cache** | In-memory bundle data for fast access |
| **Reaper** | Lifetime expiration monitoring |
| **Channels** | Hybrid fast/slow path queue management |

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────────┐
│                            Store                                     │
│                                                                      │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────────┐  │
│  │   LRU Cache     │  │  Reaper Cache   │  │  Channel Manager    │  │
│  │  (bundle data)  │  │  (expiry times) │  │  (fast/slow path)   │  │
│  └────────┬────────┘  └────────┬────────┘  └──────────┬──────────┘  │
│           │                    │                      │              │
└───────────┼────────────────────┼──────────────────────┼──────────────┘
            │                    │                      │
            ▼                    ▼                      ▼
┌─────────────────────┐  ┌─────────────────┐  ┌─────────────────────┐
│   BundleStorage     │  │ MetadataStorage │  │   Dispatcher        │
│   (trait)           │  │ (trait)         │  │                     │
├─────────────────────┤  ├─────────────────┤  │  - drop_bundle()    │
│ - localdisk-storage │  │ - sqlite-storage│  │  - ingest_bundle()  │
│ - bundle_mem        │  │ - metadata_mem  │  │  - poll_waiting()   │
└─────────────────────┘  └─────────────────┘  └─────────────────────┘
```

## Store Struct

The `Store` is the central coordinator (`src/storage/store.rs`):

```rust
pub struct Store {
    tasks: TaskPool,
    metadata_storage: Arc<dyn MetadataStorage>,
    bundle_storage: Arc<dyn BundleStorage>,
    bundle_cache: spin::Mutex<LruCache<Arc<str>, Bytes>>,
    reaper_cache: Arc<Mutex<BTreeSet<CacheEntry>>>,
    reaper_wakeup: Arc<Notify>,
    max_cached_bundle_size: usize,
    reaper_cache_size: usize,
}
```

**Lock Strategy:**
- `spin::Mutex` for bundle_cache (O(1) operations, no blocking)
- Standard `Mutex` for reaper_cache (requires O(n) iteration)

## Storage Traits

### BundleStorage

Manages binary bundle data (`src/storage/mod.rs`):

```rust
pub trait BundleStorage: Send + Sync {
    /// Recover all stored bundles (returns storage_name + timestamp)
    async fn recover(&self, tx: Sender<(Arc<str>, SystemTime)>) -> Result<()>;

    /// Load binary data by storage_name
    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>>;

    /// Save binary data, returns generated storage_name
    async fn save(&self, data: &[u8]) -> Result<Arc<str>>;

    /// Delete binary data
    async fn delete(&self, storage_name: &str) -> Result<()>;
}
```

### MetadataStorage

Manages bundle lifecycle state (`src/storage/mod.rs`):

```rust
pub trait MetadataStorage: Send + Sync {
    /// Get bundle metadata by ID
    async fn get(&self, bundle_id: &Id) -> Result<Option<Bundle>>;

    /// Insert new bundle (returns false on duplicate)
    async fn insert(&self, bundle: &Bundle) -> Result<bool>;

    /// Update existing bundle metadata
    async fn replace(&self, bundle: &Bundle) -> Result<()>;

    /// Mark as deleted (prevents re-insertion)
    async fn tombstone(&self, bundle_id: &Id) -> Result<()>;

    /// Confirm bundle exists (for recovery)
    async fn confirm_exists(&self, bundle_id: &Id) -> Result<bool>;

    /// Start recovery process
    async fn start_recovery(&self) -> Result<()>;

    /// Find orphaned bundles during restart
    async fn remove_unconfirmed(&self, tx: Sender<Bundle>) -> Result<()>;

    /// Reset bundles pending to a peer (ForwardPending → Waiting)
    async fn reset_peer_queue(&self, peer: u32) -> Result<bool>;

    /// Poll bundles by expiration time
    async fn poll_expiry(&self, tx: Sender<CacheEntry>, limit: usize) -> Result<()>;

    /// Poll bundles in Waiting status
    async fn poll_waiting(&self, tx: Sender<Bundle>) -> Result<()>;

    /// Poll bundles in specific status
    async fn poll_pending(&self, tx: Sender<Bundle>, status: &BundleStatus, limit: usize) -> Result<()>;
}
```

## Dual Storage Model

Bundle data and metadata are stored separately:

```
┌─────────────────────────────────────────────────────────────────┐
│                         Bundle                                   │
│                                                                  │
│  ┌─────────────────────────┐  ┌──────────────────────────────┐  │
│  │     Binary Data         │  │        Metadata              │  │
│  │                         │  │                              │  │
│  │  - CBOR-encoded bundle  │  │  - storage_name (pointer)    │  │
│  │  - Stored by blob key   │  │  - status (New, Waiting...)  │  │
│  │                         │  │  - received_at               │  │
│  │  Backend:               │  │  - ingress_peer_node/addr    │  │
│  │  - localdisk-storage    │  │  - flow_label                │  │
│  │  - bundle_mem           │  │                              │  │
│  │                         │  │  Backend:                    │  │
│  │                         │  │  - sqlite-storage            │  │
│  │                         │  │  - metadata_mem              │  │
│  └─────────────────────────┘  └──────────────────────────────┘  │
│                                                                  │
│  Linked by: metadata.storage_name → bundle_storage key          │
└─────────────────────────────────────────────────────────────────┘
```

### Why Separate Storage?

1. **Different access patterns**: Metadata accessed frequently (status checks), data accessed rarely (forwarding)
2. **Different backends**: SQLite for indexed queries, filesystem for large blobs
3. **Independent scaling**: Metadata on fast SSD, data on high-capacity storage
4. **Crash safety**: Atomic operations on each backend independently

## Storage Backends

### Localdisk Storage (Bundle Data)

**Location:** `/workspace/localdisk-storage/src/storage.rs`

Directory structure with 2-level hierarchy:
```
store_dir/
  XX/
    XX/
      <storage_name>
```

**Features:**
- **Atomic writes** (with `fsync=true`): Write to `.tmp`, fsync, rename, fsync directory
- **Memory-mapped loading** (with `mmap` feature): Zero-copy via `memmap2::Mmap`
- **Parallel recovery**: Thread pool walks directories concurrently

**Configuration:**
```rust
pub struct Config {
    pub store_dir: PathBuf,  // Default: /var/spool/hardy-localdisk-storage
    pub fsync: bool,         // Default: true (atomic writes)
}
```

### SQLite Storage (Metadata)

**Location:** `/workspace/sqlite-storage/src/storage.rs`

**Features:**
- Connection pool with write lock
- Prepared statement caching
- Status encoding as (code, param1, param2, param3) tuple

**Status Encoding:**
| Status | Code | Params |
|--------|------|--------|
| `New` | 0 | - |
| `Waiting` | 1 | - |
| `ForwardPending` | 2 | peer, queue |
| `AduFragment` | 3 | timestamp, sequence, source_eid |
| `Dispatching` | 4 | - |

**Configuration:**
```rust
pub struct Config {
    pub db_dir: PathBuf,
    pub db_name: String,  // Default: "metadata.db"
}
```

### In-Memory Storage (Testing)

**Bundle:** `src/storage/bundle_mem.rs` - LRU cache with capacity limit
**Metadata:** `src/storage/metadata_mem.rs` - HashMap-based

## LRU Cache Management

The Store maintains an in-memory LRU cache for frequently accessed bundle data:

```rust
bundle_cache: spin::Mutex<LruCache<Arc<str>, Bytes>>
```

### Configuration

```rust
pub struct Config {
    pub lru_capacity: NonZeroUsize,           // Default: 1024 entries
    pub max_cached_bundle_size: NonZeroUsize, // Default: 16 KB
}
```

### Cache Operations

| Operation | Strategy | Cache Behavior |
|-----------|----------|----------------|
| **Load** | Cache-first | Check cache (peek without LRU update), fall back to backend |
| **Save** | Persist-first | Always persist to backend, cache if size ≤ `max_cached_bundle_size` |
| **Delete** | Cache-then-backend | Remove from cache, then delete from backend |

See `load_data()`, `save_data()`, and `delete_data()` in `src/storage/store.rs` for implementation.

## Reaper (Expiration Monitoring)

The reaper monitors bundle lifetimes and triggers deletion on expiry (`src/storage/reaper.rs`).

### Two-Level Cache Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Reaper Cache (in-memory)                      │
│                                                                  │
│  BTreeSet<CacheEntry> ordered by expiry time                    │
│  Limited size (= poll_channel_depth)                            │
│  Keeps bundles with soonest expiry                              │
│                                                                  │
│  When full: evict entry with latest expiry (keep soonest)       │
└─────────────────────────────────┬───────────────────────────────┘
                                  │ refill when empty
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│                  MetadataStorage (persistent)                    │
│                                                                  │
│  poll_expiry(tx, limit) returns bundles ordered by expiry       │
│  Full list of all bundles with lifetimes                        │
└─────────────────────────────────────────────────────────────────┘
```

### CacheEntry Structure

```rust
struct CacheEntry {
    expiry: OffsetDateTime,
    id: bundle::Id,
    destination: Eid,
}
```

Ordered by: expiry time → destination → ID (deterministic BTreeSet ordering)

### Reaper Loop

The reaper runs as a background task with the following behavior:

1. **Sleep** until the next bundle expiry (or indefinitely if cache is empty)
2. **Wake** on: shutdown signal, new bundle notification, or expiry timeout
3. **Expire** all bundles past their lifetime via `drop_bundle()`
4. **Refill** cache from storage when depleted

The reaper uses `select_biased!` to prioritize shutdown handling. See `run_reaper()` in `src/storage/reaper.rs` for implementation.

### Watch Bundle

When a bundle enters the system, it's registered with the reaper via `watch_bundle()`:

1. Create a `CacheEntry` with expiry time, bundle ID, and destination
2. Insert into the BTreeSet cache (evicts latest expiry if full)
3. Wake the reaper if this bundle has the soonest expiry

This ensures bundles with imminent expiry are always tracked in the in-memory cache.

## Crash Recovery

Three-phase recovery process on startup (`src/storage/recover.rs`):

### Phase 1: Start Recovery

Call `start_metadata_storage_recovery()` to prepare the metadata backend:
- **SQLite**: Marks all bundle entries as "unconfirmed"
- **In-memory**: No-op

### Phase 2: Bundle Storage Recovery

Call `bundle_storage_recovery()` to scan all stored bundle data:

1. Walk storage directory, emitting `(storage_name, timestamp)` pairs
2. For each bundle, call `restart_bundle()` to determine status:

| Condition | Result | Action |
|-----------|--------|--------|
| Data + metadata exist | `Valid` | Resume from status checkpoint |
| Data exists, no metadata | `Orphan` | Insert metadata, re-ingest |
| Duplicate data found | `Duplicate` | Delete spurious copy |
| Data unparseable | `Junk` | Delete data |
| Data missing | `Missing` | Skip (race condition) |

### Phase 3: Metadata Recovery

Call `metadata_storage_recovery()` to find orphaned metadata:

1. Query for bundles still marked "unconfirmed" (data was lost)
2. Report each as deleted with `DepletedStorage` reason

See `src/storage/recover.rs` for implementation details.

### Recovery Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│ Phase 1: start_metadata_storage_recovery()                       │
│                                                                  │
│ Mark all metadata entries as "unconfirmed"                       │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│ Phase 2: bundle_storage_recovery()                               │
│                                                                  │
│ For each bundle data file:                                       │
│   ├─ Parse bundle                                                │
│   ├─ Check metadata exists?                                      │
│   │   ├─ Yes: confirm_exists(), resume from status              │
│   │   └─ No: insert metadata, re-ingest as orphan               │
│   └─ Mark metadata as "confirmed"                                │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│ Phase 3: metadata_storage_recovery()                             │
│                                                                  │
│ For each still-unconfirmed metadata entry:                       │
│   └─ Report deletion (data was lost)                            │
└─────────────────────────────────────────────────────────────────┘
```

## Crash Safety Properties

1. **Atomic store**: Bundle data saved before metadata; cleanup on failure
2. **Checkpoints**: Status transitions mark processing milestones (see [Bundle State Machine Design](bundle_state_machine_design.md))
3. **Tombstones**: Deleted bundles cannot be re-inserted
4. **Two-level verification**: Recovery cross-checks data + metadata existence
5. **Orphan detection**: Unconfirmed metadata entries reported with `DepletedStorage` reason

## Store/Load/Delete Operations

### Store (Two-Phase Atomic)

```
1. save_data(bytes)
   → bundle_storage.save(bytes) → returns storage_name
   → cache if size < max_cached_bundle_size

2. store(bundle, data)
   → save_data(data) → storage_name
   → bundle.metadata.storage_name = storage_name
   → metadata_storage.insert(bundle)
   → if insert fails (duplicate): delete_data(storage_name)
```

### Load

```
load_data(storage_name)
   → bundle_cache.peek(storage_name)? return cached
   → bundle_storage.load(storage_name)
```

### Delete

```
delete_data(storage_name)
   → bundle_cache.pop(storage_name)
   → bundle_storage.delete(storage_name)

tombstone(bundle_id)
   → metadata_storage.tombstone(bundle_id)
```

## Configuration Summary

### Store Config

```rust
pub struct Config {
    pub lru_capacity: NonZeroUsize,           // LRU cache slots (default: 1024)
    pub max_cached_bundle_size: NonZeroUsize, // Max size to cache (default: 16KB)
}
```

### Localdisk Config

```rust
pub struct Config {
    pub store_dir: PathBuf,  // Base directory
    pub fsync: bool,         // Atomic writes (default: true)
}
```

### SQLite Config

```rust
pub struct Config {
    pub db_dir: PathBuf,
    pub db_name: String,     // Default: "metadata.db"
}
```

### Bundle Memory Config

```rust
pub struct Config {
    pub capacity: NonZeroUsize,  // Max bytes (default: 256 MB)
    pub min_bundles: usize,      // Keep minimum (default: 32)
}
```

## Key Files

| File | Purpose |
|------|---------|
| `src/storage/mod.rs` | Trait definitions, Config, type aliases |
| `src/storage/store.rs` | Store struct, cache management |
| `src/storage/reaper.rs` | Expiry monitoring, BTreeSet cache |
| `src/storage/recover.rs` | Three-phase recovery |
| `src/storage/channel.rs` | Hybrid fast/slow path channels |
| `src/storage/bundle_mem.rs` | In-memory BundleStorage |
| `src/storage/metadata_mem.rs` | In-memory MetadataStorage |
| `sqlite-storage/src/storage.rs` | SQLite MetadataStorage |
| `localdisk-storage/src/storage.rs` | Filesystem BundleStorage |
