# hardy-sqlite-storage Design

SQLite-based metadata storage implementing the MetadataStorage trait.

## Design Goals

- **Serverless operation.** Use an embedded database that requires no separate server process. SQLite provides ACID guarantees in a single-file format.

- **Efficient queries.** Support fast lookup by bundle ID, polling by status, and ordering by expiry time. Index the fields used in frequent queries.

- **Concurrent access.** Allow multiple readers while supporting serialised writes. The BPA's parallel processing model requires concurrent metadata access.

- **Schema evolution.** Support database migrations when the schema changes between versions without requiring manual intervention.

## Architecture Overview

The storage layer sits between the BPA and a SQLite database file:

```
BPA
 │
 ├─ store_bundle()     ─┐
 ├─ poll_for_*()        │
 ├─ update_status()     ├──► ConnectionPool ──► SQLite (WAL mode)
 ├─ get_bundle()        │         │
 └─ remove_bundle()    ─┘         └── write_lock (serialises writes)
```

Bundle metadata is serialised as JSON and stored alongside indexed columns that enable efficient querying without deserialising every row. The connection pool manages SQLite connections, with a dedicated write lock ensuring serialised writes while allowing concurrent reads.

## Key Design Decisions

### Embedded SQLite vs External Database

SQLite was chosen over external databases (PostgreSQL, Redis) for several reasons.

It's serverless and file-based. No database server needs to be deployed, configured, or monitored. The database is a single file that can be backed up by copying.

It's embeddable. The SQLite library compiles directly into the application. There's no network communication overhead or connection establishment latency.

It has excellent tooling. Standard tools can inspect and modify the database for debugging or maintenance. The file format is stable and well-documented.

### WAL Mode for Concurrent Access

The database uses Write-Ahead Logging (WAL) mode rather than the default rollback journal. This choice directly supports the BPA's access pattern where the dispatcher reads metadata frequently while the store writes updates.

In WAL mode, readers don't block writers and writers don't block readers. Readers see a consistent snapshot even during writes. This is critical because bundle processing involves frequent metadata lookups that shouldn't stall behind write operations.

The trade-off is additional disk space for the WAL file and slightly more complex recovery, but these are acceptable given the concurrency benefits.

### Indexed Status Fields with Serialised Metadata

Bundle metadata is stored as a serialised JSON blob, but key fields (status, expiry, received_at) are duplicated in indexed columns. This duplication is intentional.

The alternative would be to deserialise every row when querying by status or expiry. For a store with thousands of bundles, this would be prohibitively slow. By extracting queryable fields into indexed columns, the database can filter efficiently and only deserialise the rows that match.

The status is encoded as a numeric code with parameters rather than as a string or JSON. Numeric comparisons are faster and indexes on integers are more compact.

### Write Serialisation

SQLite in WAL mode allows concurrent reads but requires serialised writes to avoid SQLITE_BUSY errors. Rather than relying on SQLite's busy timeout and retry logic, the pool uses an explicit Tokio mutex to queue write operations.

This provides predictable behaviour under load. Writers wait in a fair queue rather than racing with exponential backoff. The cost is that write throughput is limited to one operation at a time, but this matches SQLite's inherent single-writer constraint.

### Flyway-Inspired Migration

Schema migrations follow the pattern established by Flyway: numbered SQL files applied in sequence, with checksums to detect tampering.

Migration files are embedded at compile time, so there's no risk of missing migration scripts at runtime. Each migration's hash is stored in the database; if a hash doesn't match on startup, the storage refuses to open (indicating the schema was modified outside the migration system).

The `upgrade` configuration option allows administrators to control when migrations run. In production, this prevents unexpected schema changes during routine restarts.

## Recovery Support

The MetadataStorage trait requires a recovery protocol to synchronise metadata with bundle data after crashes. The implementation uses an "unconfirmed" table to track entries during recovery:

1. **start_recovery()** - All existing entries are marked unconfirmed
2. **confirm_exists()** - As the BPA finds bundle data on disk, it confirms each entry
3. **remove_unconfirmed()** - Entries without corresponding data are reported and removed

This protocol handles the case where a crash occurred after writing bundle data but before updating metadata, or vice versa.

## Configuration

| Option | Default | Purpose |
|--------|---------|---------|
| `db_dir` | Platform-specific | Directory containing the database file |
| `db_name` | `metadata.db` | Database filename |

Default directories follow platform conventions: `~/.cache/` on Linux, `~/Library/Caches/` on macOS, `%LOCALAPPDATA%` on Windows. Fallback paths are used when user directories aren't available.

## Integration

### With hardy-bpa

Implements the `MetadataStorage` trait. The BPA calls trait methods for bundle lifecycle management without knowing the underlying storage mechanism.

### With hardy-bpa-server

The server instantiates sqlite-storage based on configuration and injects it into the BPA.

## Dependencies

| Crate | Purpose |
|-------|---------|
| hardy-bpa | `MetadataStorage` trait definition |
| rusqlite | SQLite database access (bundled SQLite) |
| serde_json | Bundle serialisation |
| directories | Platform-specific default paths |

## Testing

- [Test Plan](test_plan.md) - SQLite metadata persistence verification
