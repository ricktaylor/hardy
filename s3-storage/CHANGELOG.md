# Changelog

All notable changes to `hardy-s3-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-06-24

### Added
- Implement the new required `BundleStorage::replace(storage_name, data)` (multipart-aware put).

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — `recover` streams via `&dyn Sender<RecoveryResponse>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.

Releases before this version predate this changelog; see the git history for details.
