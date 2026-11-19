# Changelog

All notable changes to `hardy-localdisk-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed
- **BREAKING:** `BundleStorage::replace`, removed from the trait in `hardy-bpa`: bundles are stored as received and never rewritten in place.

### Changed
- **BREAKING:** the serde `Config` struct (and the `serde` feature that gated it) and the free `new()` function are replaced by `LocalDiskStorage::new(store_dir, fsync, upgrade)` taking `Option` knobs, with the defaults owned privately by the backend; config-file schemas belong to the server crates.

### Fixed
- With `fsync` enabled, a save synced the bundle's data twice: the temporary file was opened write-through (`O_SYNC` on Unix, `FILE_FLAG_WRITE_THROUGH` on Windows), which flushed the device at the end of each write call, normally one per bundle, and was then synced again with `sync_data`. The file is now written to the page cache and synced once, before the rename, so a save makes two device flushes (the data, then the directory) where it made three; the crash-safety guarantee is unchanged. The `libc` and `winapi` dependencies go with the flags.
- With `fsync` disabled, a save created a zero-length file to claim its name, closed it, then reopened it with truncation to write the bundle. The second open made large saves several times slower than a plain write, the rewrite of a truncated file being flushed on close by filesystems such as ext4. The exclusive create that claims the name is now the file the bundle is written through, in one step; the name is protected as before, and a crash leaves what it left before, an empty or partial unreferenced file that recovery discards. With `fsync` enabled the placeholder stays: it is what stops the rename replacing a stored bundle.
- With `fsync` disabled, a save copied the whole bundle before writing it: the bytes were lent to `tokio::fs::write`, which takes ownership of a buffer it is lent by copying it, on the async worker thread. The bundle now moves to the blocking pool with the save and is written in place.
- With `fsync` enabled, a `.tmp` file left beside a free name by an earlier process failed the save that next claimed that name, and the BPA treats a failed save as fatal. The save now removes the stale file and proceeds.
- A save that failed after claiming its name left the zero-length placeholder behind, one per failed save until the next restart's recovery. Every file a failed save created is now removed before it returns.
- A save created its bucket directory, or checked that it existed, before every claim. The bucket is now created only when the claim finds it missing, which also closes a window in which the recovery walk could remove the empty bucket between its creation and the claim, failing the save.
- With `fsync` enabled, the first bundle saved into a new bucket could survive a crash while the directory entries naming its bucket did not: the bucket was synced, the directories above it were not. A save that creates its bucket now syncs those too.
- On Windows, every save with `fsync` enabled logged a failed directory sync: a directory cannot be opened as a file there. The directory sync is now skipped on Windows, which has none; the data sync is unaffected.

## [0.6.0]

### Added
- Implement the new required `BundleStorage::replace(storage_name, data)` (atomic temp-file write + rename, fsync-aware).

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — `recover` streams via `&dyn Sender<RecoveryResponse>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.
- Dropped the direct public `flume` dependency (used internally only to bridge the blocking directory walk).
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
