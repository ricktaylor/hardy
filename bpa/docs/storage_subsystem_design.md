# Storage Subsystem Design

This document describes the storage subsystem in the BPA, covering the dual storage model, caching, expiration monitoring, and crash recovery.

## Related Documents

- **[Bundle State Machine Design](bundle_state_machine_design.md)**: Bundle status values stored in metadata
- **[Filter Subsystem Design](filter_subsystem_design.md)**: Filter checkpoint persistence
- **[Policy Subsystem Design](policy_subsystem_design.md)**: Hybrid channels for queue management
- **[Routing Design](routing_subsystem_design.md)**: Route changes trigger `reset_peer_queue()`

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
│                            Store                                    │
│                                                                     │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────────┐  │
│  │   LRU Cache     │  │  Reaper Cache   │  │  Channel Manager    │  │
│  │  (bundle data)  │  │  (expiry times) │  │  (fast/slow path)   │  │
│  └────────┬────────┘  └────────┬────────┘  └──────────┬──────────┘  │
│           │                    │                      │             │
└───────────┼────────────────────┼──────────────────────┼─────────────┘
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

## Store Coordinator

The `Store` struct is the central coordinator for all storage operations. It holds references to both storage backends, manages the LRU cache and reaper cache, and coordinates recovery.

**Lock Strategy:**

- `spin::Mutex` for bundle_cache (O(1) operations, no blocking)
- Standard `Mutex` for reaper_cache (requires O(n) iteration)

## Storage Traits

The storage subsystem defines two core traits that backends must implement. See rustdoc for full API details.

### BundleStorage

The `BundleStorage` trait manages binary bundle data as opaque blobs. Implementations handle recovery (walking stored files), load/save by storage name, and deletion. The trait is designed for large data with infrequent access.

### MetadataStorage

The `MetadataStorage` trait manages bundle lifecycle state with indexed queries. Key operations include:

- **CRUD**: get, insert, replace, tombstone (prevents re-insertion)
- **Recovery**: start_recovery, confirm_exists, remove_unconfirmed
- **Queue management**: reset_peer_queue (ForwardPending → Waiting)
- **Polling**: poll_expiry, poll_waiting, poll_pending for background processing

## Dual Storage Model

Bundle data and metadata are stored separately:

```
┌─────────────────────────────────────────────────────────────────┐
│                         Bundle                                  │
│                                                                 │
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
│                                                                 │
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

Configuration includes the storage directory path and whether to use atomic writes (fsync). See the [localdisk-storage design doc](../../localdisk-storage/docs/design.md) for details.

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

Configuration includes the database directory and name. See the [sqlite-storage design doc](../../sqlite-storage/docs/design.md) for details.

### In-Memory Storage (Testing)

**Bundle:** `src/storage/bundle_mem.rs` - LRU cache with capacity limit
**Metadata:** `src/storage/metadata_mem.rs` - HashMap-based

## LRU Cache Management

The Store maintains an in-memory LRU cache for frequently accessed bundle data. Configuration controls the cache capacity (default: 1024 entries) and maximum bundle size to cache (default: 16 KB).

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
│                    Reaper Cache (in-memory)                     │
│                                                                 │
│  BTreeSet<CacheEntry> ordered by expiry time                    │
│  Limited size (= poll_channel_depth)                            │
│  Keeps bundles with soonest expiry                              │
│                                                                 │
│  When full: evict entry with latest expiry (keep soonest)       │
└─────────────────────────────────┬───────────────────────────────┘
                                  │ refill when empty
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│                  MetadataStorage (persistent)                   │
│                                                                 │
│  poll_expiry(tx, limit) returns bundles ordered by expiry       │
│  Full list of all bundles with lifetimes                        │
└─────────────────────────────────────────────────────────────────┘
```

### Cache Entries

Each cache entry tracks a bundle's expiry time, ID, and destination. The BTreeSet orders entries by expiry time → destination → ID for deterministic ordering.

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
│ Phase 1: start_metadata_storage_recovery()                      │
│                                                                 │
│ Mark all metadata entries as "unconfirmed"                      │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│ Phase 2: bundle_storage_recovery()                              │
│                                                                 │
│ For each bundle data file:                                      │
│   ├─ Parse bundle                                               │
│   ├─ Check metadata exists?                                     │
│   │   ├─ Yes: confirm_exists(), resume from status              │
│   │   └─ No: insert metadata, re-ingest as orphan               │
│   └─ Mark metadata as "confirmed"                               │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│ Phase 3: metadata_storage_recovery()                            │
│                                                                 │
│ For each still-unconfirmed metadata entry:                      │
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

## Configuration

Each storage component is configured separately:

| Component | Key Settings |
|-----------|-------------|
| **Store** | LRU cache capacity (default: 1024), max cached bundle size (default: 16 KB) |
| **Localdisk** | Storage directory, atomic writes (fsync) |
| **SQLite** | Database directory and name |
| **In-memory** | Capacity limit, minimum bundle count |

See rustdoc for `Config` structs and the respective storage backend design docs for configuration details.
