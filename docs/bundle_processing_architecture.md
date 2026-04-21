# Design: Bundle Processing Architecture

## Bundle typestates

A bundle's capabilities depend on where it is in its lifecycle. This is encoded at compile time:

```
Bundle<Idle>                          Bundle<Stored>
  carries raw data in memory            carries a storage_name key
  can be filtered, stored               can be routed, forwarded, delivered, deleted
```

```rust
pub struct Bundle<S = Idle> {
    pub bundle: Bpv7Bundle,
    pub metadata: BundleMetadata,
    pub state: S,
}

pub struct Idle { pub(crate) data: Bytes }
pub struct Stored { pub storage_name: Arc<str> }
```

The transition is a consuming method. After `store()`, the `Idle` bundle is consumed and cannot be reused:

```rust
impl Bundle<Idle> {
    pub async fn store(self, store: &Store) -> Result<Option<Bundle<Stored>>>;
}
```

Methods that apply to both states (`has_expired()`, `id()`, `payload()`) live on `impl<S> Bundle<S>`.

Methods specific to stored bundles live on `impl Bundle<Stored>`:

```rust
impl Bundle<Stored> {
    pub fn storage_name(&self) -> &Arc<str>;
    pub async fn get_data(&self, store: &Store) -> Result<Option<Bytes>>;
    pub async fn delete(self, store: &Store) -> Result<()>;
    pub async fn transition(&mut self, store: &Store, status: BundleStatus) -> Result<()>;
}
```

### Module layout

```
bundle/
  mod.rs       Bundle<S> struct, BundleMetadata, BundleStatus, generic impls, tests
  idle.rs      Idle typestate, Bundle<Idle> methods (new, data, set_data, store)
  stored.rs    Stored typestate, Bundle<Stored> methods (get_data, delete, transition)
```

## Pipeline stages

Bundle processing is a flat sequence of named stages:

```
Bpa::receive     parse CBOR, construct Bundle<Idle>, spawn into processing pool
Bpa::process     run ingress filters on in-memory data, store, transition to Dispatching
Bpa::route       RIB lookup: forward / deliver / reassemble / admin / wait
```

Filters run on `Bundle<Idle>` before storage. Rejected bundles never touch disk.

### Module layout

```
bpa/
  mod.rs          Bpa struct, key_provider, filter registration, BpaRegistration impl
  pipeline.rs     receive(), process(), route()
  admin.rs        inbound administrative record handling
  lifecycle.rs    start(), shutdown(), recover()
```

## Storage

`Store` is a thin facade that delegates to two backend traits:

- `MetadataStorage`: bundle metadata (status, ingress context, annotations)
- `BundleStorage`: raw bundle data blobs

### Cache decorator

The LRU cache is a `CachedBundleStorage` that implements `BundleStorage` by wrapping any backend. Applying the cache is a construction-time decision:

```rust
let bundle_storage: Arc<dyn BundleStorage> = match lru_capacity {
    Some(cap) => Arc::new(CachedBundleStorage::new(raw, cap, max_size)),
    None => raw,
};
let store = Store::new(reaper_size, metadata_storage, bundle_storage);
```

The cache is invisible to `Store` and to callers.

### Reaper

The expiry monitor is its own struct. It maintains a bounded BTreeSet of bundles ordered by expiry time. `Store` delegates `watch_bundle()` to it. The background task is spawned by `Store::start()`.

### Error handling

All `Store` methods return `Result`. Storage backends panic internally on unrecoverable errors (disk corruption, OOM). If an error reaches `Store`, it is recoverable and the caller decides how to handle it.

### Module layout

```
storage/
  mod.rs            MetadataStorage + BundleStorage traits, type aliases
  store.rs          Store facade
  reaper.rs         Reaper struct (expiry monitoring)
  cached.rs         CachedBundleStorage decorator
  channel.rs        Hybrid memory/storage dispatch channel
  bundle_mem.rs     In-memory BundleStorage backend
  metadata_mem.rs   In-memory MetadataStorage backend
```

## Fragmentation

Fragment reassembly is domain logic, not storage. The `Reassembler` struct holds a `&Store` and a key provider:

1. `collect()`: poll the metadata store for sibling fragments, build a `FragmentSet`
2. `stitch()`: load each fragment's payload, place at its ADU offset, rebuild the primary block without fragment info
3. Clean up fragment data and metadata
4. Parse the stitched result, store as a new `Bundle<Stored>`
5. Return `ReassemblerResult::Complete(Bundle<Stored>)` for the caller to route

`Fragment` and `FragmentSet` are shared types in `fragmentation/mod.rs`, reusable for future outbound fragmentation.

### Module layout

```
fragmentation/
  mod.rs            Fragment, FragmentSet types
  reassembler.rs    Reassembler struct (collect, stitch, run)
```

## Recovery

Recovery reconciles bundle data and metadata after an unclean shutdown. It uses a typestate pattern to enforce its three-phase protocol at compile time:

```
Recovery<Idle>       .mark()       marks all metadata as unconfirmed
Recovery<Marked>     .reconcile()  walks bundle data, confirms or re-creates metadata
Recovery<Confirmed>  .purge()      deletes orphaned metadata with no matching data
```

Each phase consumes the previous state, preventing phases from being skipped or reordered. The struct borrows `Store` and `Dispatcher` without allocation.

`confirm_exists` returns `Bundle<Stored>`, making the storage name available via the typestate rather than a metadata field.

### Module layout

```
recover/
  mod.rs          Recovery<S> struct, typestate markers, transition helper
  idle.rs         Phase 1: mark_unconfirmed
  marked.rs       Phase 2: walk storage, reconcile each bundle
  confirmed.rs    Phase 3: purge orphaned metadata
```

## Filters

Validation checks (expiry, hop count) are implemented as a `BundleValidityFilter` registered on the Ingress hook. It runs before the RFC9171 filter in the dependency chain.

```
filters/
  mod.rs          Filter traits (ReadFilter, WriteFilter), Hook enum, Error types
  filter.rs       Filter execution graph (PreparedFilters)
  registry.rs     Registry, ExecResult, Mutation
  validity.rs     BundleValidityFilter (expiry + hop count)
  rfc9171.rs      RFC9171 validity filter (integrity, bundle age)
```

## Other modules

```
registration.rs   BpaRegistration trait: public API for CLAs, services, routing agents
cbor.rs           Fast CBOR first-byte precheck before full bundle parsing
```
# Design: Bundle Processing Architecture

## Bundle typestates

A bundle's capabilities depend on where it is in its lifecycle. This is encoded at compile time:

```
Bundle<Idle>                          Bundle<Stored>
  carries raw data in memory            carries a storage_name key
  can be filtered, stored               can be routed, forwarded, delivered, deleted
```

```rust
pub struct Bundle<S = Idle> {
    pub bundle: Bpv7Bundle,
    pub metadata: BundleMetadata,
    pub state: S,
}

pub struct Idle { pub(crate) data: Bytes }
pub struct Stored { pub storage_name: Arc<str> }
```

The transition is a consuming method. After `store()`, the `Idle` bundle is consumed and cannot be reused:

```rust
impl Bundle<Idle> {
    pub async fn store(self, store: &Store) -> Result<Option<Bundle<Stored>>>;
}
```

Methods that apply to both states (`has_expired()`, `id()`, `payload()`) live on `impl<S> Bundle<S>`.

Methods specific to stored bundles live on `impl Bundle<Stored>`:

```rust
impl Bundle<Stored> {
    pub fn storage_name(&self) -> &Arc<str>;
    pub async fn get_data(&self, store: &Store) -> Result<Option<Bytes>>;
    pub async fn delete(self, store: &Store) -> Result<()>;
    pub async fn transition(&mut self, store: &Store, status: BundleStatus) -> Result<()>;
}
```

### Module layout

```
bundle/
  mod.rs       Bundle<S> struct, BundleMetadata, BundleStatus, generic impls, tests
  idle.rs      Idle typestate, Bundle<Idle> methods (new, data, set_data, store)
  stored.rs    Stored typestate, Bundle<Stored> methods (get_data, delete, transition)
```

## Pipeline stages

Bundle processing is a flat sequence of named stages:

```
Bpa::receive     parse CBOR, construct Bundle<Idle>, spawn into processing pool
Bpa::process     run ingress filters on in-memory data, store, transition to Dispatching
Bpa::route       RIB lookup: forward / deliver / reassemble / admin / wait
```

Filters run on `Bundle<Idle>` before storage. Rejected bundles never touch disk.

### Module layout

```
bpa/
  mod.rs          Bpa struct, key_provider, filter registration, BpaRegistration impl
  pipeline.rs     receive(), process(), route()
  admin.rs        inbound administrative record handling
  lifecycle.rs    start(), shutdown(), recover()
```

## Storage

`Store` is a thin facade that delegates to two backend traits:

- `MetadataStorage`: bundle metadata (status, ingress context, annotations)
- `BundleStorage`: raw bundle data blobs

### Cache decorator

The LRU cache is a `CachedBundleStorage` that implements `BundleStorage` by wrapping any backend. Applying the cache is a construction-time decision:

```rust
let bundle_storage: Arc<dyn BundleStorage> = match lru_capacity {
    Some(cap) => Arc::new(CachedBundleStorage::new(raw, cap, max_size)),
    None => raw,
};
let store = Store::new(reaper_size, metadata_storage, bundle_storage);
```

The cache is invisible to `Store` and to callers.

### Reaper

The expiry monitor is its own struct. It maintains a bounded BTreeSet of bundles ordered by expiry time. `Store` delegates `watch_bundle()` to it. The background task is spawned by `Store::start()`.

### Error handling

All `Store` methods return `Result`. Storage backends panic internally on unrecoverable errors (disk corruption, OOM). If an error reaches `Store`, it is recoverable and the caller decides how to handle it.

### Module layout

```
storage/
  mod.rs            MetadataStorage + BundleStorage traits, type aliases
  store.rs          Store facade
  reaper.rs         Reaper struct (expiry monitoring)
  cached.rs         CachedBundleStorage decorator
  channel.rs        Hybrid memory/storage dispatch channel
  bundle_mem.rs     In-memory BundleStorage backend
  metadata_mem.rs   In-memory MetadataStorage backend
```

## Fragmentation

Fragment reassembly is domain logic, not storage. The `Reassembler` struct holds a `&Store` and a key provider:

1. `collect()`: poll the metadata store for sibling fragments, build a `FragmentSet`
2. `stitch()`: load each fragment's payload, place at its ADU offset, rebuild the primary block without fragment info
3. Clean up fragment data and metadata
4. Parse the stitched result, store as a new `Bundle<Stored>`
5. Return `ReassemblerResult::Complete(Bundle<Stored>)` for the caller to route

`Fragment` and `FragmentSet` are shared types in `fragmentation/mod.rs`, reusable for future outbound fragmentation.

### Module layout

```
fragmentation/
  mod.rs            Fragment, FragmentSet types
  reassembler.rs    Reassembler struct (collect, stitch, run)
```

## Recovery

Recovery reconciles bundle data and metadata after an unclean shutdown. It uses a typestate pattern to enforce its three-phase protocol at compile time:

```
Recovery<Idle>       .mark()       marks all metadata as unconfirmed
Recovery<Marked>     .reconcile()  walks bundle data, confirms or re-creates metadata
Recovery<Confirmed>  .purge()      deletes orphaned metadata with no matching data
```

Each phase consumes the previous state, preventing phases from being skipped or reordered. The struct borrows `Store` and `Dispatcher` without allocation.

`confirm_exists` returns `Bundle<Stored>`, making the storage name available via the typestate rather than a metadata field.

### Module layout

```
recover/
  mod.rs          Recovery<S> struct, typestate markers, transition helper
  idle.rs         Phase 1: mark_unconfirmed
  marked.rs       Phase 2: walk storage, reconcile each bundle
  confirmed.rs    Phase 3: purge orphaned metadata
```

## Filters

Validation checks (expiry, hop count) are implemented as a `BundleValidityFilter` registered on the Ingress hook. It runs before the RFC9171 filter in the dependency chain.

```
filters/
  mod.rs          Filter traits (ReadFilter, WriteFilter), Hook enum, Error types
  filter.rs       Filter execution graph (PreparedFilters)
  registry.rs     Registry, ExecResult, Mutation
  validity.rs     BundleValidityFilter (expiry + hop count)
  rfc9171.rs      RFC9171 validity filter (integrity, bundle age)
```

## Other modules

```
registration.rs   BpaRegistration trait: public API for CLAs, services, routing agents
cbor.rs           Fast CBOR first-byte precheck before full bundle parsing
```
