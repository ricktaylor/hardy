# Changelog

All notable changes to `hardy-postgres-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `forward_ack_pending` bundle status (migration 0002), the `reset_peer_ack_pending` sweep, and the status-conditioned `swap_status`/`tombstone_if`, for the deferred CLA transfer-outcome extension.
- `dispatch_pending`, `deliver_pending`, and `delivery_ack_pending` bundle statuses (migration 0004) with the `deliver_pending` per-service partial index (migration 0005), and the `reset_service_queue` sweep, for the BPA's dispatch/delivery queue rationalisation. Migration 0006 adds the `dispatch_pending` partial index the dispatch queue's storage poller pages by, and drops the read-dead `dispatching` index it succeeds.

### Changed
- Records (de)serialize through `hardy-bpa`'s `StoredBundle`/`StoredBundleRef` — the on-disk format is unchanged, and the status is re-imposed from the typed columns by construction.
- `MetadataStorage::update_status` is gone (removed from the `hardy-bpa` trait): every persisted status transition after first dispatch is a conditional compare-and-swap.
- `poll_expiry` streams keyset pages until the consumer closes the stream, per the revised `hardy-bpa` trait contract (the `limit` parameter is gone); the page budget no longer caps the scan.
- **BREAKING:** the persisted record format changed in `hardy-bpa` (the wire-bundle key rename `bundle` → `bpv7` and the new required `origin` provenance key). Records written by earlier versions no longer deserialize: recovery treats each as corrupt and tombstones it, and the restart re-ingest then discards the orphaned bundle data as duplicates against the permanent `bundles` identity anchor. The schema-checksum validation cannot catch this — the blob inside the schema is unversioned. Wipe the metadata database when upgrading a node with a populated store — restart then re-ingests the bundle store cleanly.
- **BREAKING:** the serde `Config` struct and the free `new()` function are replaced by `PostgresStorage::builder()`, with the pool defaults owned privately by the builder; config-file schemas belong to the server crates. Timeouts are `Duration`s, `poll_page_size` and `max_connections` are `NonZeroU32` (a zero-connection pool is unrepresentable), and a missing database URL is the dedicated `Error::NoDatabaseUrl`.

## [0.2.0]

### Changed
- **BREAKING:** adopt the `hardy_bpa::stream::Sender` push-trait — the streaming `MetadataStorage` methods (`remove_unconfirmed`, `poll_expiry`, `poll_waiting`, `poll_service_waiting`) take `&dyn Sender<Bundle>` instead of a `flume::Sender`; requires `hardy-bpa` 0.2.
- Bumped `sqlx` 0.8 → 0.9 (internal; adapted to the new `Migrate::ensure_migrations_table`/`list_applied_migrations` API).
- Raised the minimum supported Rust version (MSRV) to 1.95.

Releases before this version predate this changelog; see the git history for details.
