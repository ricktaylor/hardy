# Changelog

All notable changes to `hardy-bpa` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-06-24

### Added
- `BpaBuilder::key_provider()` and `service_priority()`; BPSec `KeyProvider` key-resolution wiring.
- New `stream` module exposing the push-side `Sender<T>` trait and `SendError<T>`, with a blanket impl for `hardy_async::channel::Sender`.
- New public `filter` types: `Mutation`, `ExecResult`, and the `filter::validity` submodule.
- `BundleStorage::replace` for atomic in-place overwrite; `storage` re-exports `BundleMemStorage`, `MetadataMemStorage`, `CachedBundleStorage` and their `*Config` types.
- `critical-section` cargo feature (forwarded to `hardy-bpv7`) for targets without native 64-bit atomics.

### Changed
- **BREAKING:** renamed module `routes` → `routing` and its `Action` enum → `RouteAction`; `RoutingSink::add_route`/`remove_route` take `RouteAction`. Added `Error::NullNextHop` and `Error::ViaOwnNode`.
- **BREAKING:** renamed module `filters` → `filter`; renamed `FilterResult` → `ReadResult` and `RewriteResult` → `WriteResult` (its `Continue` payload is now `Option<Vec<u8>>`, was `Option<Box<[u8]>>`). `Bpa::register_filter`/`unregister_filter` return `filter::Error`.
- **BREAKING:** `MetadataStorage`/`BundleStorage` streaming methods (`recover`, `remove_unconfirmed`, `poll_*`) take `&dyn stream::Sender<T>` instead of a `flume::Sender<T>` by value — every storage backend must be updated.
- Switched the node-id RNG from `ThreadRng` to a `SysRng`-seeded `SmallRng`; moved internal channels off `flume` to `hardy-async`/`arc-swap`.

### Removed
- `cla::Error::InvalidBundle(hardy_bpv7::Error)` variant.
- Public `storage::Sender<T> = flume::Sender<T>` alias and the public `storage::bundle_mem`/`storage::metadata_mem` modules (reach them via the re-exports above).
- `NodeIds::resolve_eid` is now crate-private.

Releases before this version predate this changelog; see the git history for details.
