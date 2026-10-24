# Changelog

All notable changes to `hardy-async` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `signal` feature: gates the `signal` module and enables tokio's signal handling. Implies `tokio`.
- `Semaphore` and `OwnedSemaphorePermit`: a runtime-agnostic permit budget shared between tasks (over `tokio::sync::Semaphore`), whose clones share one budget; it is never closed, so acquiring cannot fail. The BPA's egress lanes are such a budget, shared by a peer's queue pollers.

### Changed
- **BREAKING:** `signal::listen_for_cancel` now requires the `signal` feature; callers must enable it. The `tokio` feature no longer enables tokio's `signal` feature, so builds without `signal` no longer pull in tokio's signal driver and mio.

## [0.2.0]

### Added
- `channel` module: runtime-agnostic bounded/unbounded MPMC channels (`Sender`, `Receiver`, `bounded`, `unbounded`) over flume, gated on the `std` feature.
- `closeable` module: an explicitly-closeable channel variant with `close()` and shared error types.
- `watcher` module (new `watcher` feature): runtime-agnostic filesystem watcher (`WatchMode`, `watch()`).
- `Notify::notify_waiters()` to wake all current waiters; `Once::wait()` to spin until a cell is initialised.

### Changed
- **BREAKING:** bumped `spin` 0.10 → 0.12. `sync::spin` re-exports the spin guard types, and `Mutex::lock`/`try_lock` now return `MutexGuard<'_, T, spin::Spin>` — breaking for callers naming the re-exported guard types or pinning `spin`.
- Made the file watcher runtime-agnostic and decoupled its config from the runtime watch mode.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
