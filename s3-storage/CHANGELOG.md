# Changelog

All notable changes to `hardy-s3-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **BREAKING**: the serde `Config` struct (and the `serde` feature that gated it) and the free `new()` function are replaced by `S3Storage::builder(bucket)`, with the multipart defaults owned privately by the builder; config-file schemas belong to the server crates. The part size is the `PartSize` newtype, making the S3 5 MiB minimum unrepresentable, and a multipart threshold below the part size is rejected at build (previously an unenforced doc claim). The construction errors are the dedicated `EmptyBucket` and `ThresholdBelowPartSize` variants.

## [0.2.0]

### Added
- Implement the new required `BundleStorage::replace(storage_name, data)` (multipart-aware put).

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — `recover` streams via `&dyn Sender<RecoveryResponse>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
