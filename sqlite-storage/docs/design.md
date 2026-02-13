# hardy-sqlite-storage Design

SQLite-based metadata storage implementing the MetadataStorage trait.

## Design Goals

- **Serverless operation.** Use an embedded database that requires no separate server process. SQLite provides ACID guarantees in a single-file format.

- **Efficient queries.** Support fast lookup by bundle ID, polling by status, and ordering by expiry time. Index the fields used in frequent queries.

- **Concurrent access.** Allow multiple readers while supporting serialised writes. The BPA's parallel processing model requires concurrent metadata access.

- **Schema evolution.** Support database migrations when the schema changes between versions without requiring manual intervention.

## Why SQLite

SQLite is well-suited for this use case for several reasons.

It's serverless and file-based. No database server needs to be deployed, configured, or monitored. The database is a single file that can be backed up by copying.

It's embeddable. The SQLite library compiles directly into the application. There's no network communication overhead or connection establishment latency.

It has excellent tooling. Standard tools can inspect and modify the database for debugging or maintenance. The file format is stable and well-documented.

## Storage Schema

Bundle metadata is stored as a serialised blob with indexed fields extracted for querying:

```sql
CREATE TABLE bundles (
    id INTEGER PRIMARY KEY,
    bundle_id BLOB NOT NULL UNIQUE,
    expiry TEXT NOT NULL,
    received_at TEXT NOT NULL,
    status_code INTEGER,
    status_param1 INTEGER,
    status_param2 INTEGER,
    status_param3 TEXT,
    bundle BLOB
) STRICT;

CREATE INDEX idx_bundles_expiry ON bundles(expiry ASC);
CREATE INDEX idx_bundles_status ON bundles(status_code);
CREATE INDEX idx_bundles_status_peer ON bundles(status_code, status_param1);
CREATE INDEX idx_bundles_received_at ON bundles(received_at ASC);
```

The composite index `idx_bundles_status_peer` optimises queries filtering by status and peer ID, such as `reset_peer_queue()` and `poll_pending()` for ForwardPending bundles. The `idx_bundles_received_at` index supports FIFO ordering in poll operations.

The full bundle (including metadata) is serialised with `serde_json` and stored in the `bundle` column. Key fields (status, expiry, received_at) are duplicated in indexed columns to enable efficient querying without deserialising every row.

### Auxiliary Tables

Two auxiliary tables support recovery and queue operations:

```sql
-- Tracks bundles pending confirmation during recovery
CREATE TABLE unconfirmed_bundles (
    id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE
) STRICT;

-- Maintains FIFO ordering for Waiting bundles
CREATE TABLE waiting_queue (
    id INTEGER UNIQUE NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
    received_at TEXT NOT NULL
) STRICT;

CREATE INDEX idx_waiting_queue_received_at ON waiting_queue(received_at ASC);
```

### Status Encoding

Bundle status is encoded as a numeric code with up to three parameters:

| Status | Code | Parameters |
|--------|------|------------|
| `New` | 0 | none |
| `Waiting` | 1 | none |
| `ForwardPending` | 2 | peer_id, queue |
| `AduFragment` | 3 | timestamp, sequence, source_eid |
| `Dispatching` | 4 | none |

This encoding allows status-based queries (e.g., "all bundles in Waiting status") without deserialising the full metadata.

## Connection Pooling

SQLite connections are managed in a pool to avoid open/close overhead:

```rust
struct ConnectionPool {
    path: PathBuf,
    connections: Mutex<Vec<Connection>>,
    write_lock: tokio::sync::Mutex<()>,
}
```

The pool maintains a stack of ready connections. When a connection is needed, one is popped from the stack; when released, it's pushed back. If the pool is empty, a new connection is created.

### Write Serialisation

SQLite in WAL mode allows concurrent reads but requires serialised writes. The pool uses a `tokio::sync::Mutex` to ensure only one write operation proceeds at a time. Reads acquire connections without this lock, enabling read/write concurrency.

## WAL Mode

The database uses Write-Ahead Logging (WAL) mode rather than the default rollback journal:

- **Readers don't block writers.** Multiple read transactions can proceed while a write is in progress.
- **Writers don't block readers.** Readers see a consistent snapshot even during writes.
- **Better write performance.** Writes append to the WAL file rather than modifying the database in place.

WAL mode is especially beneficial for the BPA's access pattern where the dispatcher reads metadata frequently while the store writes updates.

## Schema Migration

The migration scheme is inspired by the popular [Flyway](https://flywaydb.org/) tool. Migration SQL files are stored in `schemas/` and processed at build time into a Rust array. Each migration is hashed, allowing runtime verification that the expected migrations were applied to an existing database.

On startup, the storage checks the database schema version and applies any necessary migrations:

1. If the database doesn't exist, create it with the latest schema
2. If the schema version is older and `upgrade` is enabled, apply sequential migrations
3. If the schema version is older and `upgrade` is disabled, fail with `UpdateRequired`
4. If the schema version is newer, fail (can't downgrade)
5. If historic migrations have different hashes, fail (schema tampering detected)

Migrations are versioned and applied in order. Each migration transforms the schema from version N to version N+1. The `upgrade` parameter is passed from the BPA configuration, allowing administrators to control when schema changes are applied.

## Recovery Support

The MetadataStorage trait requires recovery operations:

- **start_recovery()** - Mark all entries as "unconfirmed" at the start of recovery
- **confirm_exists()** - Mark an entry as "confirmed" when the corresponding bundle data is found
- **remove_unconfirmed()** - After recovery, report and remove entries without corresponding data

This protocol ensures metadata and bundle data stay synchronised after crashes.

## Configuration

| Option | Default | Purpose |
|--------|---------|---------|
| `db_dir` | Platform-specific (see below) | Directory containing the database file |
| `db_name` | `metadata.db` | Database filename |

### Platform Defaults

| Platform | Default Path |
|----------|--------------|
| Linux | `~/.cache/hardy-sqlite-storage` |
| macOS | `~/Library/Caches/dtn.Hardy.hardy-sqlite-storage` |
| Windows | `%LOCALAPPDATA%\Hardy\hardy-sqlite-storage\cache` |

Fallback paths when user directories aren't available:
- Unix: `/var/spool/hardy-sqlite-storage`
- Windows: `hardy-sqlite-storage` in executable directory

## Integration

### With hardy-bpa

This library implements the `MetadataStorage` trait defined in hardy-bpa. The BPA calls trait methods for bundle lifecycle management without knowing the underlying storage mechanism.

### With hardy-bpa-server

The server instantiates sqlite-storage based on configuration and injects it into the BPA. Configuration options (database directory, database name) come from the server's config file.

## Dependencies

Feature flags control optional functionality:

- **`tracing`**: Span instrumentation for async operations.

Key external dependencies:

| Crate | Purpose |
|-------|---------|
| hardy-bpa | `MetadataStorage` trait definition |
| rusqlite | SQLite database access (bundled SQLite) |
| serde_json | Bundle serialization |
| directories | Platform-specific default paths |
| tokio | Async runtime integration |

## Testing

- [Test Plan](test_plan.md) - SQLite metadata persistence verification
