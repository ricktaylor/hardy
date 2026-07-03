# Changelog

All notable changes to `hardy-async` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0]

### Added
- `channel` module: runtime-agnostic bounded/unbounded MPMC channels (`Sender`, `Receiver`, `bounded`, `unbounded`) over flume, gated on the `std` feature.
- `closeable` module: an explicitly-closeable channel variant with `close()` and shared error types.
- `watcher` module (new `watcher` feature): runtime-agnostic filesystem watcher (`WatchMode`, `watch()`).
- `Notify::notify_waiters()` to wake all current waiters; `Once::wait()` to spin until a cell is initialised.

### Changed
- **BREAKING:** bumped `spin` 0.10 → 0.12. `sync::spin` re-exports the spin guard types, and `Mutex::lock`/`try_lock` now return `MutexGuard<'_, T, spin::Spin>` — breaking for callers naming the re-exported guard types or pinning `spin`.
- Made the file watcher runtime-agnostic and decoupled its config from the runtime watch mode.

Releases before this version predate this changelog; see the git history for details.
