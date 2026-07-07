# Changelog

All notable changes to `hardy-localdisk-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0]

### Added
- Implement the new required `BundleStorage::replace(storage_name, data)` (atomic temp-file write + rename, fsync-aware).

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — `recover` streams via `&dyn Sender<RecoveryResponse>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.
- Dropped the direct public `flume` dependency (used internally only to bridge the blocking directory walk).
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
