# Changelog

All notable changes to `hardy-bpa` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Deferred CLA transfer outcomes (see [Deferred CLA Transfer Outcomes](docs/design.md#deferred-cla-transfer-outcomes)): `ForwardBundleResult::Accepted` lets a CLA take ownership of a transfer and report `Delivered`/`Failed` later via the new `Sink::transfer_outcome`, keyed by bundle ID. Accepted bundles are retained in the new `BundleStatus::ForwardAckPending` state until the outcome arrives, the peer is removed (outcome-unknown, back to `Waiting`), or the bundle expires. A deferred `Failed` re-enters dispatch per-bundle rather than resetting the whole peer queue.
- `MetadataStorage::reset_peer_ack_pending` — the outcome-unknown sweep, mirroring `reset_peer_queue`.

### Changed
- **BREAKING:** `Cla::forward` takes the bundle ID alongside the bundle bytes, so a deferring CLA can echo it back without parsing the bundle. `ForwardBundleResult` and `BundleStatus` have new variants; `Sink` has a new required method.
- The dispatcher records `ForwardAckPending` before offering a bundle to the CLA, so an in-flight transfer is distinguishable from a queued one and a deferred outcome cannot race the offer.

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
