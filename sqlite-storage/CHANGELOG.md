# Changelog

All notable changes to `hardy-sqlite-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **BREAKING:** the serde `Config` struct and the free `new()` function are replaced by `SqliteStorage::new(db_dir, db_name, upgrade)` taking `Option` knobs, with the defaults owned privately by the backend; config-file schemas belong to the server crates. The connection pool moves into its own module.
- Run all connections at `PRAGMA synchronous = NORMAL` (previously the SQLite default, `FULL`). Under WAL this stops fsyncing the log on every commit — a significant win on fsync-expensive storage, since the metadata store commits on each bundle status transition. Consistency across a crash is unaffected; at most the un-checkpointed tail of commits is lost, which restart recovery already tolerates (bundle data storage is ground truth, and data whose metadata is missing is re-ingested at startup).

### Added
- `forward_ack_pending` status encoding (code 6), the `reset_peer_ack_pending` sweep, and the status-conditioned `swap_status`/`tombstone_if`, for the deferred CLA transfer-outcome extension.

### Fixed
- A status update or tombstone for a concurrently deleted bundle logs at debug rather than error: delete is terminal and the write quietly loses.

## [0.6.0]

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — the streaming `MetadataStorage` methods (`remove_unconfirmed`, `poll_expiry`, `poll_waiting`, `poll_service_waiting`) take `&dyn Sender<Bundle>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.
- Removed the direct `flume` dependency.
- Bumped `rusqlite` 0.39 → 0.40 (internal; no public-API impact).
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Enable WAL journal mode at connection setup. The `PRAGMA journal_mode = WAL` in the initial schema never took effect — journal mode cannot be changed inside the migration transaction, and SQLite refuses silently — so databases were running with the default rollback journal, serialising readers behind write commits. Existing databases are converted on first open.

Releases before this version predate this changelog; see the git history for details.
