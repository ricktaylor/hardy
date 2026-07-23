# Changelog

All notable changes to `hardy-postgres-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `forward_ack_pending` bundle status (migration 0002) and the `reset_peer_ack_pending` sweep, for the deferred CLA transfer-outcome extension.

## [0.2.0]

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — the streaming `MetadataStorage` methods (`remove_unconfirmed`, `poll_expiry`, `poll_waiting`, `poll_service_waiting`) take `&dyn Sender<Bundle>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.
- Bumped `sqlx` 0.8 → 0.9 (internal; adapted to the new `Migrate::ensure_migrations_table`/`list_applied_migrations` API).
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
