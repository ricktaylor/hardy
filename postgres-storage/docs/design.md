# hardy-postgres-storage Design

PostgreSQL-based storage implementing **`MetadataStorage` only**.

## Design Goals

- **Async-native operation.** All I/O is non-blocking. No blocking database calls on async paths, no thread offloading required.

- **Correct concurrency.** Leverage PostgreSQL's MVCC instead of application-level write serialization. Multiple BPA instances can share the same database safely.

- **Metadata-only footprint.** No `BYTEA` payload columns. The `metadata` table is compact and cache-friendly; all poll queries are single-table scans over typed columns and partial indexes.

- **Production-grade.** Built for multi-node DTN deployments where high availability, backups, and point-in-time recovery are managed at the database tier.

- **Schema evolution.** Numbered migration files applied in sequence, with checksums to detect tampering.

## Architecture Overview

```mermaid
graph LR
    BPA --> MetadataStorage --> sqlx::PgPool --> PostgreSQL
```

`MetadataStorage` is implemented by a single `Storage` struct backed by a `sqlx::PgPool`. The pool manages connections transparently; there is no application-level write lock.

The implementation lives in a single `metadata.rs` module, which owns both the `bundles` (identity anchor) and `metadata` (lifecycle state) tables.

## Key Design Decisions

### `sqlx` as the Database Driver

`sqlx` is chosen over raw `tokio-postgres`:

- **Compile-time query verification.** The `query!` family of macros checks SQL syntax and column types against a live database at compile time, eliminating an entire class of runtime errors.
- **Async-native.** No need to offload blocking calls to `tokio::task::spawn_blocking`. All operations use async I/O directly.
- **Built-in connection pooling.** `sqlx::PgPool` handles health checks, idle timeouts, and back-pressure. No manual pool implementation needed.
- **Built-in migrations.** `sqlx migrate run` applies numbered `.sql` files and tracks checksums in `_sqlx_migrations`.

### MVCC over Write Serialization

SQLite's single-writer constraint forced an explicit `tokio::sync::Mutex` to queue writes. PostgreSQL uses Multi-Version Concurrency Control: concurrent writers work on isolated snapshots, conflicts are resolved by the engine, and read-write contention is eliminated.

Consequences:
- No `write_lock` field in `Storage`.
- `INSERT ... ON CONFLICT DO NOTHING` for idempotent inserts (replaces SQLite's `INSERT OR IGNORE`).
- `UPDATE ... WHERE id = $1` is safe under concurrent workloads.
- `poll_waiting` uses a `REPEATABLE READ` transaction for snapshot consistency (see below).

### Schema: `bundles` Identity Anchor, `metadata` Child

A bundle identity is committed to `bundles` exactly once, before any lifecycle state is assigned. `metadata` *describes* a bundle in flight; its FK points to `bundles`. This split keeps semantics explicit and unambiguous:

| State | `bundles` row | `metadata` row |
|---|---|---|
| Active | present | present |
| Tombstoned | present | absent |
| Never seen | absent | absent |

The `UNIQUE` constraint on `bundles.bundle_id` enforces deduplication and prevents resurrection after tombstone — no additional guard is needed in application code.

**`insert()` is atomic.** Both the `bundles` identity row and the `metadata` child row are created in a single CTE. `ON CONFLICT DO NOTHING` on `bundles.bundle_id` turns a duplicate insert into a clean false return.

**Tombstone mechanics.** When a bundle is tombstoned, the `metadata` row is deleted but the `bundles` row is kept permanently. Its `UNIQUE` constraint then prevents any future `insert()` for the same bundle identity.

### Status Representation

SQLite encodes status as a numeric code with up to three opaque parameters. PostgreSQL offers a richer type system; status is represented as a typed `ENUM` with dedicated columns per parameter:

```sql
CREATE TYPE bundle_status AS ENUM (
    'new',
    'waiting',
    'forward_pending',
    'adu_fragment',
    'dispatching',
    'waiting_for_service'
);
```

Each status variant has its own nullable columns (`peer_id`, `queue_id`, `adu_source`, `adu_ts_ms`, `adu_ts_seq`, `service_eid`). This removes the magic-number mapping, makes queries self-documenting, and enables partial indexes per status variant.

### Bundle Blob as JSONB

The `metadata.bundle` column stores the full `hardy_bpa::bundle::Bundle` struct — not just the RFC fields, but also `BundleMetadata` (status, received_at, ingress peer, flow label, `storage_name`). This is the form the BPA works with internally; storing it whole avoids re-parsing CBOR and re-deriving BPA state on every read.

It is stored as `JSONB` rather than `BYTEA` or plain `TEXT` because:

- Stores data more compactly than text JSON.
- Supports GIN indexes on JSON paths if future queries need them.
- Allows `pg_dump` to produce human-readable output.
- Enables `psql` inspection without external tools.

It is **not** a normalized relational representation of the RFC bundle structure. Every poll query in this design filters on `status`, `expiry`, and `received_at` — fields from `BundleMetadata`, not from the RFC block structure. The database never needs to look inside the RFC fields.

The JSONB blob is the authoritative source for deserialization on `get()` and all poll calls; the typed columns (`status`, `expiry`, `peer_id`, etc.) are projections of `BundleMetadata` fields, duplicated only so the database can index and filter without parsing JSONB on every row.

### Snapshot Polling via `REPEATABLE READ`

SQLite's `poll_waiting` uses an auxiliary `waiting_queue` table as a stable cursor. PostgreSQL's MVCC provides a cleaner mechanism: open a `REPEATABLE READ` transaction once, then issue batched keyset-paginated reads within it. Every read in the transaction sees the same snapshot — bundles added after the poll started are invisible. This removes the auxiliary table entirely.

Keyset pagination avoids `OFFSET` scans:
```sql
SELECT id, received_at, bundle FROM metadata
WHERE status = 'waiting'
  AND (received_at, id) > ($last_received_at, $last_id)
ORDER BY received_at ASC, id ASC
LIMIT 64
```

The `(received_at, id)` pair is a stable, collision-free cursor because `id` is a unique sequence. This is O(log n) per page versus O(n) for `OFFSET`.

### Partial Indexes

Every polling query filters on `status`. Standard indexes on status have poor selectivity when most rows are `waiting` or `dispatching`. Partial indexes on `metadata` target specific variants:

```sql
CREATE INDEX ON metadata (expiry ASC)
    WHERE status != 'new';

CREATE INDEX ON metadata (received_at ASC, id ASC)
    WHERE status = 'waiting';

CREATE INDEX ON metadata (peer_id, received_at ASC)
    WHERE status = 'forward_pending';

CREATE INDEX ON metadata (adu_source, adu_ts_ms, adu_ts_seq)
    WHERE status = 'adu_fragment';

CREATE INDEX ON metadata (service_eid, received_at ASC)
    WHERE status = 'waiting_for_service';
```

Each partial index is smaller than a full-table index and is used exclusively by its matching query plan.

## Schema

### `bundle_status` Enum

```sql
CREATE TYPE bundle_status AS ENUM (
    'new',
    'waiting',
    'forward_pending',
    'adu_fragment',
    'dispatching',
    'waiting_for_service'
);
```

### `bundles` Table

Identity anchor and tombstone guard. A row exists here for every bundle identity ever seen — active or tombstoned. The `UNIQUE` constraint on `bundle_id` is the deduplication and resurrection-prevention mechanism.

```sql
CREATE TABLE bundles (
    id          BIGSERIAL   PRIMARY KEY,

    -- BPv7 bundle identity. UNIQUE enforces deduplication and tombstone.
    bundle_id   BYTEA       NOT NULL UNIQUE,  -- JSON-encoded BPv7 bundle::Id

    -- Ingress time, set at insert(). Never NULL.
    received_at TIMESTAMPTZ NOT NULL
);
```

### `metadata` Table

Owns all lifecycle state. Absent for tombstoned bundles. Contains no `BYTEA` columns so all metadata scans are compact. `received_at` is denormalized from `bundles` so that poll queries with keyset pagination remain single-table without a join.

```sql
CREATE TABLE metadata (
    -- Same id as the parent bundles row.
    id          BIGINT      PRIMARY KEY REFERENCES bundles (id),

    -- Temporal metadata. received_at is denormalized from bundles for poll index efficiency.
    expiry      TIMESTAMPTZ NOT NULL,
    received_at TIMESTAMPTZ NOT NULL,

    -- Status (never NULL in an active metadata row)
    status      bundle_status NOT NULL,

    -- Status parameters (NULL when not applicable to the current status)
    peer_id     INTEGER,            -- ForwardPending.peer
    queue_id    INTEGER,            -- ForwardPending.queue
    adu_source  TEXT,               -- AduFragment.source (EID string)
    adu_ts_ms   BIGINT,             -- AduFragment.timestamp (milliseconds)
    adu_ts_seq  BIGINT,             -- AduFragment.sequence_number
    service_eid TEXT,               -- WaitingForService.service (EID string)

    -- Full bundle metadata blob.
    -- Stores the complete hardy_bpa::bundle::Bundle struct as JSONB.
    -- Typed columns above are projections of fields within this blob.
    bundle      JSONB NOT NULL
);

-- Partial indexes for polling queries (see Key Design Decisions)
CREATE INDEX idx_metadata_expiry
    ON metadata (expiry ASC)
    WHERE status != 'new';

CREATE INDEX idx_metadata_waiting
    ON metadata (received_at ASC, id ASC)
    WHERE status = 'waiting';

CREATE INDEX idx_metadata_forward_pending
    ON metadata (peer_id, received_at ASC)
    WHERE status = 'forward_pending';

CREATE INDEX idx_metadata_adu_fragment
    ON metadata (adu_source, adu_ts_ms, adu_ts_seq)
    WHERE status = 'adu_fragment';

CREATE INDEX idx_metadata_service_waiting
    ON metadata (service_eid, received_at ASC)
    WHERE status = 'waiting_for_service';
```

### `unconfirmed` Table

The recovery protocol (start_recovery → confirm_exists → remove_unconfirmed) requires tracking which metadata rows have not yet been confirmed. `ON DELETE CASCADE` from `metadata` ensures the entry is automatically cleaned up when its metadata row is tombstoned.

```sql
CREATE TABLE unconfirmed (
    id  BIGINT  NOT NULL UNIQUE
                REFERENCES metadata (id) ON DELETE CASCADE
);
```

## MetadataStorage Implementation

### `insert(bundle)`

Creates the `bundles` identity row and the `metadata` child row atomically in a single CTE. `ON CONFLICT DO NOTHING` on `bundles.bundle_id` handles duplicates (including post-tombstone re-insertion attempts) and turns them into a clean `false` return.

```sql
WITH ins_bundle AS (
    INSERT INTO bundles (bundle_id, received_at)
    VALUES ($bundle_id, $received_at)
    ON CONFLICT (bundle_id) DO NOTHING
    RETURNING id
)
INSERT INTO metadata (id, expiry, received_at, status,
                      peer_id, queue_id, adu_source,
                      adu_ts_ms, adu_ts_seq, service_eid, bundle)
SELECT $id, $expiry, $received_at, $status,
       $peer_id, $queue_id, $adu_source,
       $adu_ts_ms, $adu_ts_seq, $service_eid, $bundle
FROM ins_bundle
```

Returns `true` (1 row inserted into `metadata`) on success; `false` (0 rows) when `bundle_id` already exists in `bundles`.

### `get(bundle_id)`

Looks up by bundle identity via a join:

```sql
SELECT m.bundle
FROM metadata m
JOIN bundles b ON m.id = b.id
WHERE b.bundle_id = $bundle_id
```

Returns `None` for unknown or tombstoned identities.

### `replace(bundle)`

Updates the `metadata` row in place. Typed projection columns are updated alongside the JSONB blob to keep indexes current:

```sql
UPDATE metadata
SET status      = $status,
    peer_id     = $peer_id,
    queue_id    = $queue_id,
    adu_source  = $adu_source,
    adu_ts_ms   = $adu_ts_ms,
    adu_ts_seq  = $adu_ts_seq,
    service_eid = $service_eid,
    bundle      = $bundle
WHERE id = (SELECT id FROM bundles WHERE bundle_id = $bundle_id)
```

### `tombstone(bundle_id)`

Deletes the `metadata` row. The `bundles` row is kept permanently; its `UNIQUE` constraint continues to block re-insertion:

```sql
DELETE FROM metadata
WHERE id = (SELECT id FROM bundles WHERE bundle_id = $bundle_id)
```

### Polling Operations

All poll operations follow the same pattern: open a `REPEATABLE READ` transaction, then issue batched keyset-paginated reads until the page is empty. The transaction is committed (or rolled back on error) after the last page.

`poll_waiting` example:
```sql
SELECT id, received_at, bundle FROM metadata
WHERE status = 'waiting'
  AND (received_at, id) > ($last_received_at, $last_id)
ORDER BY received_at ASC, id ASC
LIMIT 64
```

`poll_expiry`, `poll_pending`, `poll_adu_fragments`, and `poll_service_waiting` use analogous queries filtered on their respective partial indexes.

## Recovery Protocol

The three-phase recovery protocol follows the BPA's `MetadataStorage` contract. On startup the BPA calls `confirm_exists` for each bundle it finds in storage. Metadata rows not confirmed within a recovery session are orphans and are tombstoned.

1. **`start_recovery()`** — Mark all active metadata rows as unconfirmed:
   ```sql
   INSERT INTO unconfirmed (id)
   SELECT id FROM metadata
   ON CONFLICT DO NOTHING
   ```

2. **`confirm_exists(bundle_id)`** — Called by the BPA for each bundle found in storage. Reads the metadata and removes the unconfirmed marker:
   ```sql
   -- Look up by bundle identity
   SELECT m.id, m.bundle
   FROM metadata m
   JOIN bundles b ON m.id = b.id
   WHERE b.bundle_id = $1
   LIMIT 1;

   -- Remove from unconfirmed if present
   DELETE FROM unconfirmed WHERE id = $id;
   ```
   Returns the bundle metadata if the row exists, or `None` if absent.

3. **`remove_unconfirmed(tx)`** — Stream and tombstone unconfirmed rows in batches. The loop terminates when the batch is empty:
   ```sql
   -- One atomic statement per batch.
   WITH batch AS (
       DELETE FROM unconfirmed
       WHERE id IN (SELECT id FROM unconfirmed LIMIT 64)
       RETURNING id
   ),
   snapshot AS (
       SELECT m.id, m.bundle
       FROM metadata m
       JOIN batch ON m.id = batch.id
   ),
   clear_meta AS (
       DELETE FROM metadata WHERE id IN (SELECT id FROM batch)
   )
   SELECT bundle FROM snapshot
   ```
   Break when the result set is empty; send each bundle to `tx` before the next batch.

## Configuration

| Option              | Default                    | Purpose                                     |
|---------------------|----------------------------|---------------------------------------------|
| `database_url`      | `$DATABASE_URL` env var    | PostgreSQL connection string                |
| `max_connections`   | `10`                       | Maximum pool size                           |
| `min_connections`   | `1`                        | Minimum idle connections                    |
| `connect_timeout`   | `30s`                      | Maximum time to acquire a connection        |
| `idle_timeout`      | `10m`                      | Time before idle connections are closed     |
| `upgrade`           | `false`                    | Run pending migrations on startup           |

Connection strings follow the standard PostgreSQL URI format:
```
postgres://user:password@host:5432/dbname?sslmode=require
```

All PostgreSQL connection parameters (SSL modes, client certificates, application name, `search_path`, etc.) are configured via the connection string rather than separate config fields.

## Migrations

Migration files live in `migrations/` and are embedded at compile time using `sqlx::migrate!`. Each file is named `{version}_{description}.sql` (e.g. `0001_setup.sql`). `sqlx` stores migration history and checksums in `_sqlx_migrations`.

If a checksum mismatches on startup, the storage refuses to open. The `upgrade` option gates migration application: when `false`, the storage refuses to open if unapplied migrations exist. This prevents accidental schema changes during rolling restarts.

## Concurrency Model

`MetadataStorage` methods execute concurrently on different pool connections. There is no application-level write lock or ordering constraint.

```mermaid
graph LR
    poll["MetadataStorage::poll_waiting()"] --> sel["SELECT metadata"] --> snap1["MVCC snapshot"] --> ret["Returns"]
    ins["MetadataStorage::insert()"] --> insB["INSERT bundles\nINSERT metadata"] --> snap2["MVCC snapshot"] --> com1["Commit (CTE, atomic)"]
    rep["MetadataStorage::replace()"] --> upd["UPDATE metadata"] --> snap3["MVCC snapshot"] --> com2["Commit"]
    tomb["MetadataStorage::tombstone()"] --> del["DELETE metadata"] --> snap4["MVCC snapshot"] --> com3["Commit"]
```

All operations run under `READ COMMITTED` isolation except:

- `poll_waiting` (and all other poll operations) use `REPEATABLE READ` for snapshot stability across keyset-paginated batches.
- `remove_unconfirmed` uses `READ COMMITTED` with a multi-CTE statement to batch atomically.

### Concurrency Guarantees

| Scenario | Outcome |
|----------|---------|
| Two concurrent `insert()` calls for the same `bundle_id` | `ON CONFLICT DO NOTHING` ensures exactly one row lands in `bundles`; the loser's `metadata` insert sees no `ins_bundle` row and returns `false`. |
| `poll_waiting()` concurrent with `replace()` | `poll_waiting` holds a `REPEATABLE READ` snapshot; status changes committed after the snapshot started are invisible for that poll cycle. |
| `tombstone()` concurrent with `get()` | `get()` may return `None` if `tombstone()` commits first; this is the correct observable state. |
| `tombstone()` followed by `insert()` for same `bundle_id` | `ON CONFLICT DO NOTHING` in `insert()` finds the existing `bundles` row and returns `false`. Resurrection is prevented. |

## Integration

### With `hardy-bpa`

Implements `MetadataStorage`. The BPA receives this as its metadata backend. The `storage_name` (a pointer into whichever bundle storage backend is in use) is stored inside the `metadata.bundle` JSONB blob as part of `BundleMetadata`.

### With `hardy-bpa-server`

The server instantiates `Storage::new(config).await?` and injects it into the BPA as the metadata backend.

## Dependencies

| Crate             | Purpose                                             |
|-------------------|-----------------------------------------------------|
| `hardy-bpa`       | `MetadataStorage` trait definition                  |
| `sqlx`            | Async PostgreSQL driver, connection pooling, migrations |
| `serde_json`      | Bundle struct serialization for the JSONB blob      |
| `thiserror`       | Typed error definitions                             |
| `tokio`           | Async runtime                                       |
| `trace_err`       | Error context tracing                               |

## Testing

Key test scenarios:

- **PG-01 Basic CRUD**: insert, get, replace, tombstone round-trip.
- **PG-02 Idempotent insert**: two concurrent inserts for the same `bundle_id` both return without error; exactly one row exists.
- **PG-03 Recovery**: insert a bundle, call `start_recovery()`, skip `confirm_exists` for it, call `remove_unconfirmed`; verify the orphan is streamed and tombstoned.
- **PG-04 poll_waiting snapshot**: insert bundle, start poll, update bundle status mid-poll; verify snapshot excludes the update.
- **PG-05 Keyset pagination**: store 200 `waiting` bundles; verify `poll_waiting` delivers all 200 in `received_at` order across multiple pages.
- **PG-06 Concurrent writers**: 10 tasks each inserting distinct bundles concurrently; verify all 10 are present with no deadlocks or constraint violations.
- **PG-07 Migration checksum**: tamper with a migration row in `_sqlx_migrations`; verify storage refuses to open.
- **PG-08 Tombstone semantics**: tombstone a bundle, then attempt re-insert; verify the second insert returns `false`.
- **PG-09 Tombstone deletes metadata**: tombstone a fully inserted bundle; verify the `metadata` row is deleted and the `bundles` row is retained.
