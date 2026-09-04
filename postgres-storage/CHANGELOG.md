# Changelog

All notable changes to `hardy-postgres-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `forward_ack_pending` bundle status (migration 0002), the `reset_peer_ack_pending` sweep, and the status-conditioned `swap_status`/`tombstone_if`, for the deferred CLA transfer-outcome extension.
- `dispatch_pending`, `deliver_pending`, and `delivery_ack_pending` bundle statuses (migration 0004) with the `deliver_pending` per-service partial index (migration 0005), and the `reset_service_queue` sweep, for the BPA's dispatch/delivery queue rationalisation.

### Changed
- **BREAKING:** the serde `Config` struct and the free `new()` function are replaced by `PostgresStorage::builder()`, with the pool defaults owned privately by the builder; config-file schemas belong to the server crates. Timeouts are `Duration`s, `poll_page_size` and `max_connections` are `NonZeroU32` (a zero-connection pool is unrepresentable), and a missing database URL is the dedicated `Error::NoDatabaseUrl`.

### Fixed
- `update_status` no longer errors when the bundle was deleted concurrently: delete is terminal and the update quietly loses. Previously the error propagated into the BPA's fail-stop storage wrapper, turning a benign race with the expiry reaper into a panic.

## [0.2.0]

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — the streaming `MetadataStorage` methods (`remove_unconfirmed`, `poll_expiry`, `poll_waiting`, `poll_service_waiting`) take `&dyn Sender<Bundle>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.
- Bumped `sqlx` 0.8 → 0.9 (internal; adapted to the new `Migrate::ensure_migrations_table`/`list_applied_migrations` API).
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
