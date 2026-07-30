# Changelog

All notable changes to `hardy-bpa` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **BREAKING:** `BundleMetadata` is partitioned by write discipline. Arrival facts are a write-once provenance group read via `received_at()` / `origin()`, with the new `Origin` enum (`Ingress { peer_node, peer_addr, cla }` / `Originated` / `Recovered`); the decoded extension-field cache moves to the public `wire: WireCache` group (was `read_only: ReadOnlyMetadata`); `ReadOnlyMetadata` is removed. `Default` is removed — construct records via `ingress()` / `originated()` / `new(received_at, origin)`.
- **BREAKING:** the persisted metadata serde shape changes: `origin` is a new (tagged) required key replacing the flattened `ingress_peer_node` / `ingress_peer_addr` keys, and the ingress CLA name is now persisted inside it — fixing restart recovery, which previously mis-read every recovered bundle as locally originated because the CLA name was transient. Metadata stores written by earlier versions do not deserialize.
- A reassembled ADU now carries the provenance of its earliest-arriving fragment (previously it had no ingress context at all).

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
