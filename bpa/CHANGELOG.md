# Changelog

All notable changes to `hardy-bpa` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `BpaBuilder::max_bundle_size` — bounds streamed reassembly at the shared concat chokepoint (private 64 MiB default); truncated streams now surface `cla::Error::StreamCancelled` so a CLA can withhold its transfer acknowledgement.
- Deferred CLA transfer outcomes (see [Deferred CLA Transfer Outcomes](docs/design.md#deferred-cla-transfer-outcomes)): `ForwardBundleResult::Accepted` lets a CLA take ownership of a transfer and report `Completed`/`Failed` later via the new `Sink::transfer_outcome`, keyed by bundle ID. Accepted bundles are retained in the new `BundleStatus::ForwardAckPending` state until the outcome arrives, the peer is removed (outcome-unknown, back to `Waiting`), or the bundle expires. A deferred `Failed` re-enters dispatch per-bundle rather than resetting the whole peer queue. Outcome resolution is arbitrated by the new status-conditioned `MetadataStorage::swap_status` and its terminal form `tombstone_if` (a completed transfer resolves straight to its tombstone, never transiting a status the dispatch poller could recover), so an outcome racing the peer-loss sweep, bundle expiry, or a duplicate of itself is ignored; the in-memory metadata backend additionally never replaces a tombstone with a live entry.
- `MetadataStorage::reset_peer_ack_pending` — the outcome-unknown sweep, mirroring `reset_peer_queue`.
- `Bytes` implements `stream::Receiver<Segment>` — a whole in-memory buffer is a one-`Final`-segment stream, zero-copy — and `stream::buffer_stream` assembles a segment stream into a contiguous buffer, enforcing `total_len` exactness and 32-bit addressability via the new `stream::BufferError` (which converts into `cla::Error`/`services::Error` through the new `PayloadUnaddressable` variants).

### Changed
- An application send no longer loops rebuilding on a duplicate bundle id: `hardy-bpv7`'s `CreationTimestamp::now` is process-monotonic, so a builder-made id cannot collide with an id this process issued — the Builder ensures uniqueness rather than the caller checking for it. `services::Error::DuplicateBundle` from an application send now only means a collision with a pre-restart bundle (a wall clock that stepped backwards across a restart) and is surfaced to the caller.
- **BREAKING:** `cla::Sink::dispatch` takes the bundle as a segment stream (`&mut dyn stream::Receiver<Segment>`), bounded by `max_bundle_size`; a CLA holding a whole bundle in memory passes it directly, since `Bytes` implements `stream::Receiver<Segment>`.
- **BREAKING:** `ServiceSink::send` takes the bundle as a segment stream, bounded by `max_bundle_size` like CLA ingress. `services::Error` gains `StreamCancelled` for a stream that ends before its final segment.
- **BREAKING:** the service delivery callbacks are renamed to `on_deliver`, one streamed door per trait: `Service::on_deliver` and `Application::on_deliver` receive the bundle ID, the exact `total_len`, and the payload as a segment stream (buffer it with `stream::buffer_stream` when the whole payload is needed in memory). The `source` parameter is gone — it is the bundle ID's source component.
- **BREAKING:** `stream::Receiver` is `Send`-only (the `Sync` bound is dropped) and `recv` takes `&mut self`, making exclusive consumption a type-system property and implementors plain state machines.
- **BREAKING:** `Cla::queue_count` is replaced by `lane_count(&self) -> Option<NonZeroUsize>` with no default — lanes are the CLA-side parallel transport channels (queues remain the BPA-side priority queues feeding them), and `None` declares unbounded parallelism. Eager per-lane queue instantiation is clamped to 256 so a CLA's declaration cannot size an unbounded allocation.
- **BREAKING:** `BpaBuilder` is obtained only from `Bpa::builder()`: `BpaBuilder::new` is no longer public and the `Default` impl is removed. The cache setters (`lru_capacity`, `max_cached_bundle_size`) have no effect when no bundle storage is configured, as the default memory store is never cached.
- **BREAKING:** `Cla::forward` takes an optional lane, the bundle ID (so a deferring CLA can echo it back without parsing the bundle), the exact `total_len`, and the bundle as a segment stream. `ForwardBundleResult` and `BundleStatus` have new variants; `Sink` has a new required method.
- The dispatcher records `ForwardAckPending` before offering a bundle to the CLA, so an in-flight transfer is distinguishable from a queued one and a deferred outcome cannot race the offer.
- **BREAKING:** the library no longer exposes config-file structs; config schemas belong to the server crates. `MetadataMemStorage::new` and `BundleMemStorage::new` take `Option<NonZeroUsize>` knobs with dimension-named parameters (the zero floor is unrepresentable instead of silently clamped), `Rfc9171ValidityFilter` is configured through fluent setters, and defaults are owned privately at the point of use; the `MetadataMemStorageConfig`/`BundleMemStorageConfig` re-exports and `filter::rfc9171::Config` are removed.
- **BREAKING:** the bundle-cache defaults are owned by `CachedBundleStorage`: its `new` takes `Option` knobs and applies its own defaults, and the `DEFAULT_LRU_CAPACITY`/`DEFAULT_MAX_CACHED_BUNDLE_SIZE` constants are no longer public. `BpaBuilder` carries unset knobs as `None` instead of materialising defaults, and `build` caches any configured bundle storage unless `no_cache()` was called; the default in-memory storage is never cached.

### Fixed
- Overlapping service-registration polls (a re-registering service, or a poll racing an application cancel) can no longer dispatch — and potentially deliver — the same bundle twice: the poll claims each bundle out of `WaitingForService` with a status-conditioned swap.
- A duplicate bundle copy delivered by the hybrid channels' at-least-once storage recovery can no longer re-enter circulation or produce a second CLA offer: a queue move (`storage::channel::Sender::send`) is now a status-conditioned swap from the sender's snapshot, and forwarding claims the bundle out of its peer queue the same way before offering it — a duplicate dispatch copy could previously stomp an in-flight `ForwardAckPending` back to `ForwardPending`, re-arming the peer queue mid-transfer, and a duplicate egress copy could offer the same bundle twice.

### Removed
- **BREAKING:** `ServiceSink::cancel` and `ApplicationSink::cancel` — the implementation was status-blind, reporting success for a bundle already mid-transfer at a CLA. A future cancellation must be conditional on a still-cancellable status; the contract is recorded in [docs/TODO.md](docs/TODO.md).

## [0.2.0]

### Added
- `BpaBuilder::key_provider()` and `service_priority()`; BPSec `KeyProvider` key-resolution wiring.
- New `stream` module exposing the push-side `Sender<T>` trait and `SendError<T>`, with a blanket impl for `hardy_async::channel::Sender`.
- New public `filter` types: `Mutation`, `ExecResult`, and the `filter::validity` submodule.
- `BundleStorage::replace` for atomic in-place overwrite; `storage` re-exports `BundleMemStorage`, `MetadataMemStorage`, `CachedBundleStorage` and their `*Config` types.
- `critical-section` cargo feature (forwarded to `hardy-bpv7`) for targets without native 64-bit atomics.
- `cla::Error::PayloadTooLarge { size, max }` and `services::Error::PayloadTooLarge { size, max }` for pre-flight rejection of over-sized bundles/payloads before they can break a transport stream.

### Changed
- **BREAKING:** renamed module `routes` → `routing` and its `Action` enum → `RouteAction`; `RoutingSink::add_route`/`remove_route` take `RouteAction`. Added `Error::NullNextHop` and `Error::ViaOwnNode`.
- **BREAKING:** renamed module `filters` → `filter`; renamed `FilterResult` → `ReadResult` and `RewriteResult` → `WriteResult` (its `Continue` payload is now `Option<Vec<u8>>`, was `Option<Box<[u8]>>`). `Bpa::register_filter`/`unregister_filter` return `filter::Error`.
- **BREAKING:** `MetadataStorage`/`BundleStorage` streaming methods (`recover`, `remove_unconfirmed`, `poll_*`) take `&dyn stream::Sender<T>` instead of a `flume::Sender<T>` by value — every storage backend must be updated.
- Switched the node-id RNG from `ThreadRng` to a `SysRng`-seeded `SmallRng`; moved internal channels off `flume` to `hardy-async`/`arc-swap`.
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Preserve bundles when a service delivery returns an error: the failure is propagated so the bundle is re-queued for retry instead of being dropped.
- Treat unexpired tombstones as dedup state and drop already-expired bundles at ingress, before the metadata write.
- `MetadataMemStorage` evicts tombstones before live bundles; `BundleMemStorage` uses an edge-triggered capacity watermark with corrected eviction.
- Exit the storage `poll_queue` drain promptly on cancellation instead of running one extra poll cycle.

### Removed
- `cla::Error::InvalidBundle(hardy_bpv7::Error)` variant.
- Public `storage::Sender<T> = flume::Sender<T>` alias and the public `storage::bundle_mem`/`storage::metadata_mem` modules (reach them via the re-exports above).
- `NodeIds::resolve_eid` is now crate-private.

Releases before this version predate this changelog; see the git history for details.
