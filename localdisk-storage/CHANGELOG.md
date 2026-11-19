# Changelog

All notable changes to `hardy-localdisk-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed
- **BREAKING:** `BundleStorage::replace`, removed from the trait in `hardy-bpa`: bundles are stored as received and never rewritten in place.

### Changed
- **BREAKING:** the serde `Config` struct (and the `serde` feature that gated it) and the free `new()` function are replaced by `LocalDiskStorage::new(store_dir, fsync, upgrade)` taking `Option` knobs, with the defaults owned privately by the backend; config-file schemas belong to the server crates.

### Fixed
- With `fsync` enabled, a save synced the bundle's data twice: the temporary file was opened write-through (`O_SYNC` on Unix, `FILE_FLAG_WRITE_THROUGH` on Windows), which flushed the device on every write, and was then synced again with `sync_data`. The file is now written buffered and synced once, before the rename; the crash-safety guarantee is unchanged. The `libc` and `winapi` dependencies go with the flags.
- With `fsync` disabled, a save created a zero-length file to claim its name, closed it, then reopened it with truncation to write the bundle. The second open made large saves several times slower than a plain write, the rewrite of a truncated file being flushed on close by filesystems such as ext4. The exclusive create that claims the name is now the file the bundle is written through, in one step; the name is protected as before, and a crash leaves what it left before, an empty or partial unreferenced file that recovery discards. With `fsync` enabled the placeholder stays: it is what stops the rename replacing a stored bundle.

## [0.6.0]

### Added
- Implement the new required `BundleStorage::replace(storage_name, data)` (atomic temp-file write + rename, fsync-aware).

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — `recover` streams via `&dyn Sender<RecoveryResponse>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.
- Dropped the direct public `flume` dependency (used internally only to bridge the blocking directory walk).
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
