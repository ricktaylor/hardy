# Changelog

All notable changes to `hardy-sqlite-storage` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Migration `02_poll_pending_index`: the covering index `idx_bundles_status_received (status_code, status_param1, status_param2, received_at, status_param3)` serves the `poll_pending` page query in index order — previously every drain page top-K-sorted the entire matching backlog through a temp B-tree, an O(B²) drain under sustained overload, on the node-wide dispatch queue and every per-service delivery queue. The superseded `idx_bundles_status` and `idx_bundles_status_peer` (both strict prefixes of the new index) are dropped, and a plan pin test asserts the poll shape never sorts out of index.
- Migration `03_forward_pending_repark`: a `forward_pending` row now persists its resolved adjacency in `status_param3` (matched back per row by the queue poll, not filtered on). A row without one predates the change and has no recoverable queue assignment, so the migration re-parks it to `waiting` — letting it surface adjacency-less would drop it as garbage and destroy the bundle at the restart sweep; the next dispatch pass re-routes it instead.
- Migration `04_live_row_indexes`: the two secondary indexes become partial indexes that hold live bundles only (`idx_bundles_expiry ... WHERE bundle IS NOT NULL`, `idx_bundles_status_received ... WHERE status_code IS NOT NULL`), and `idx_bundles_received_at`, which no query used, is dropped. A bundle's lifetime now touches about half as many index pages, which roughly doubles peak write throughput; a tombstone takes about 290 bytes instead of about 440; and the reaper's expiry poll no longer walks tombstones (with a million of them ahead of the first live bundle, 1.9 ms instead of 330 ms). The migration rebuilds both indexes when the store is opened with upgrades enabled: about 0.6 s per million rows. It does not shrink the database file; the freed pages are reused.
- `forward_ack_pending` status encoding (code 6), the `reset_peer_ack_pending` sweep, and the status-conditioned `swap_status`/`tombstone_if`, for the deferred CLA transfer-outcome extension.
- `dispatch_pending` (code 7), `deliver_pending` (code 8), and `delivery_ack_pending` (code 9) status encodings and the `reset_service_queue` sweep, for the BPA's dispatch/delivery queue rationalisation.

### Changed
- Records (de)serialize through `hardy-bpa`'s `StoredBundle`/`StoredBundleRef` — the on-disk format is unchanged, and the status is re-imposed from the typed columns by construction.
- `MetadataStorage::update_status` is gone (removed from the `hardy-bpa` trait): every persisted status transition after first dispatch is a conditional compare-and-swap.
- `MetadataStorage::replace` is gone (removed from the `hardy-bpa` trait): a record's non-status metadata is fixed at insert.
- `poll_expiry` pages with a keyset cursor `(expiry, rowid)` and streams until the consumer closes the stream, per the revised `hardy-bpa` trait contract (the `limit` parameter is gone).
- **BREAKING:** the persisted record format changed in `hardy-bpa` (the wire-bundle key rename `bundle` → `bpv7` and the new required `origin` provenance key). Rows written by earlier versions no longer deserialize: recovery logs each as "Garbage bundle found in metadata" and tombstones it, and the restart re-ingest then discards the orphaned bundle data as duplicates of the tombstoned rows. Wipe the metadata database when upgrading a node with a populated store — restart then re-ingests the bundle store cleanly.
- Tracking the `hardy-bpa` forwarding-queue change, `forward_pending` (code 2) encodes the resolved next-hop adjacency EID in `status_param3` (previously unused for this code, so no schema change): the pending-poll for a forwarding queue matches on queue identity (`status_param1`/`status_param2`) and emits each row's own adjacency, and the peer-queue reset clears `status_param3`.
- **BREAKING:** the serde `Config` struct and the free `new()` function are replaced by `SqliteStorage::new(db_dir, db_name, upgrade)` taking `Option` knobs, with the defaults owned privately by the backend; config-file schemas belong to the server crates.
- Run all connections at `PRAGMA synchronous = NORMAL` (previously the SQLite default, `FULL`). Under WAL this stops fsyncing the log on every commit — a significant win on fsync-expensive storage, since the metadata store commits on each bundle status transition. Consistency across a crash is unaffected; at most the un-checkpointed tail of commits is lost, which restart recovery already tolerates (bundle data storage is ground truth, and data whose metadata is missing is re-ingested at startup).
- Every write runs on a dedicated writer thread that owns the store's only writing connection, in place of a Tokio mutex around pooled connections. Writes that queue while the thread is busy commit together in one transaction, each in its own savepoint, so a failing write rolls back alone; each caller is answered only after its batch has committed. Reads run on Tokio's blocking thread pool over a bounded pool of query-only connections. No SQLite call blocks an async worker thread any more. With dozens of concurrent callers, metadata throughput roughly doubles. A strictly sequential caller pays about 0.1 ms more per operation for the thread hand-off, which nearly halves its throughput; the two break even at between one and four concurrent callers.
- Write-ahead log checkpoints run on a thread of their own, in place of SQLite's automatic checkpoint inside a commit. The writer no longer stops for about 20 ms each time the WAL passes 4 MiB, with every queued write waiting behind it; it finishes a checkpoint itself only once the WAL passes 32 MiB, for under 10 ms, and waits for the checkpoint thread only if the WAL reaches 64 MiB. Under a steady 3,000 writes per second, 99th-percentile write latency falls from about 16 ms to under 1 ms, and peak throughput rises by a further two thirds. The WAL file now grows to as much as 64 MiB (previously about 4 MiB).

### Fixed
- A write that makes SQLite roll back the whole transaction (a full disk, an I/O error) fails every write in its batch, and no later write in the batch is run. It could not happen before the writer thread batched writes; within this release the writes after such a failure ran outside any transaction and committed, while every caller in the batch was told it had failed.
- `confirm_exists` treats a tombstoned row as absent (`Ok(None)`) instead of failing on its NULL columns. Previously a bundle-data blob whose metadata row was tombstoned — a crash between tombstone and data deletion leaves exactly that — made startup recovery panic on every boot, an unrecoverable crash loop cleared only by manual database surgery; the row now matches the same `bundle IS NOT NULL` predicate every other tombstone-aware query uses, recovery re-ingests the blob as an orphan, the insert reports it as a duplicate of the tombstone, and the stranded data is deleted — the store self-heals.
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
