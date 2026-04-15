# Recovery

Hardy is designed to recover gracefully from crashes, power failures, and unexpected restarts. This page explains what happens when the server restarts, how each storage backend handles failures, and what operator actions may be needed.

## Restart and Recovery

Hardy separates **metadata storage** (bundle state, timestamps, status) from **bundle data storage** (the raw bytes). These two stores can become inconsistent if the process crashes between writing to one and the other.

On every startup, Hardy runs a three-phase recovery protocol to bring them back into sync:

1. **Mark** -- all metadata entries are marked as _unconfirmed_.
2. **Confirm** -- the bundle data store is walked. For each bundle found, the corresponding metadata entry is confirmed.
3. **Cleanup** -- any metadata entry still unconfirmed is deleted, because its bundle data is missing.

After recovery completes, every metadata entry is guaranteed to have matching bundle data. The server then begins normal operation.

This protocol is idempotent. If the server crashes during recovery itself, the next startup will simply run the protocol again from the beginning.

## Bundle Data Backends

### Local Disk

The local disk backend stores each bundle as a file on the filesystem.

**Save operation:**

Saving a bundle is a six-step process designed so that a crash at any point leaves the system in a recoverable state:

1. Create a 0-byte placeholder file with a unique random name.
2. Rename the placeholder to a `.tmp` extension.
3. Write the bundle data to the `.tmp` file.
4. Sync the file data to disk (`fsync`).
5. Rename the `.tmp` file to its final filename. This is an atomic operation on POSIX filesystems.
6. Sync the parent directory to make the rename durable.

**What happens if the system crashes:**

| Crash point | State on disk | What happens on restart |
|---|---|---|
| During steps 1-4 | A `.tmp` or 0-byte file exists | The file is detected and deleted during recovery |
| After step 5, before step 6 | File has its final name but the directory is not synced | On most modern journaling filesystems the file will be visible. In rare cases it may be lost. |
| After step 6 | File is fully durable | Nothing to do |

If a crash occurs during any of the first four steps, the bundle data is lost but the metadata store will detect the missing bundle during the confirm phase and clean up its own entry. No manual intervention is needed.

**Recovery walk:** On startup, the store directory is walked. All `.tmp` files and 0-byte placeholders are deleted. Empty directories are removed. A timestamp fence ensures that files being written by concurrent operations are not accidentally deleted. Surviving files are reported to the metadata store for confirmation.

**The `fsync` option:**

- `fsync: true` (default): writes use `O_SYNC` and both file data and directory metadata are explicitly synced. This is the safest option.
- `fsync: false`: no sync calls are made. The OS may buffer writes. A crash can lose data that has not yet been flushed to disk. Use this only when performance matters more than durability (e.g. in-memory tmpfs, or when bundles can be retransmitted).

**Delete:** A single `remove_file` call. If the file is already gone, the error is ignored.

**Operator actions:** None required. Recovery is fully automatic.

### S3

The S3 backend stores each bundle as an object in an S3-compatible object store.

**Small bundles** (below `multipart-threshold`, default 8 MiB):

A single `PutObject` call. From S3's perspective this is atomic. If the process crashes before the call completes, the object is never created. No cleanup is needed.

**Large bundles** (at or above `multipart-threshold`):

Large bundles use S3 multipart upload:

1. `CreateMultipartUpload` -- S3 returns an upload ID.
2. Upload each part sequentially with `UploadPart`.
3. `CompleteMultipartUpload` -- makes the object visible.

If the process crashes during step 2, Hardy attempts a best-effort `AbortMultipartUpload`. However, if the process is killed before the abort can execute, the incomplete multipart upload remains in S3.

**What happens if the system crashes:**

| Crash point | State in S3 | What happens on restart |
|---|---|---|
| Before `CreateMultipartUpload` | Nothing | Nothing to do |
| During part uploads | Incomplete multipart upload exists but is invisible to `ListObjects` | Hardy does not clean this up automatically |
| After `CompleteMultipartUpload` | Object is complete and visible | Nothing to do |

**Recovery walk:** Lists all objects with the configured prefix and reports each one to the metadata store for confirmation.

**Operator actions:**

Incomplete multipart uploads are invisible to Hardy's recovery protocol and will accumulate over time if not cleaned up. Configure an [S3 lifecycle rule](https://docs.aws.amazon.com/AmazonS3/latest/userguide/mpu-abort-incomplete-mpu-lifecycle-config.html) to automatically expire incomplete uploads. For example, a rule that aborts incomplete uploads after 1 day:

```json
{
  "Rules": [
    {
      "ID": "abort-incomplete-uploads",
      "Status": "Enabled",
      "Filter": { "Prefix": "" },
      "AbortIncompleteMultipartUpload": {
        "DaysAfterInitiation": 1
      }
    }
  ]
}
```

## Metadata Backends

### SQLite

SQLite uses Write-Ahead Logging (WAL) mode. All writes are serialized through an async mutex.

**Insert:** A single `INSERT OR IGNORE` statement. If the process crashes before the transaction commits, SQLite's WAL recovery rolls it back and the bundle is not in the database. If it crashes after commit, the data is durable.

**Tombstone** (bundle forwarded or expired): The bundle data column is set to `NULL` but the `bundle_id` row is preserved. This prevents duplicate bundles from being re-inserted. If the process crashes before commit, the tombstone is rolled back and the metadata remains intact.

**Recovery protocol:**

1. `start_recovery()` inserts all active bundle IDs into an `unconfirmed_bundles` table. This uses `INSERT OR IGNORE`, so it is safe to re-run if the process crashes during this step.
2. `confirm_exists()` is called for each bundle found in the bundle data store. It removes the bundle from the `unconfirmed_bundles` table and returns its metadata.
3. `remove_unconfirmed()` deletes any remaining entries from `unconfirmed_bundles` in batches, using atomic CTEs to snapshot the metadata before deletion.

**What happens if the system crashes during recovery:**

| Crash point | Effect | What happens on next restart |
|---|---|---|
| During `start_recovery()` | Partially populated `unconfirmed_bundles` table | Safe to re-run (`INSERT OR IGNORE`) |
| During `confirm_exists()` | Some bundles not yet confirmed | They remain in `unconfirmed_bundles` and will be cleaned up in the next cleanup phase |
| During `remove_unconfirmed()` | Metadata may be deleted from the database while the bundle data still exists in the bundle store | The orphaned bundle data will be cleaned up on the next recovery walk |

**Operator actions:** None required. You can also use the `--upgrade-store` and `--recover-store` CLI flags to upgrade or repair the SQLite database:

- `--upgrade-store` (`-u`): upgrades the database schema to the current format.
- `--recover-store` (`-r`): attempts to recover damaged records.

### PostgreSQL

Uses standard PostgreSQL transactions. Write operations are single statements with implicit transactions at `READ COMMITTED` isolation. Read operations use `REPEATABLE READ READ ONLY` snapshot transactions for consistency.

**Insert:** An atomic CTE that creates both the `bundles` row and the `metadata` row in a single statement. `ON CONFLICT DO NOTHING` deduplicates by `bundle_id`. If the process crashes before the statement completes, nothing is written.

**Tombstone:** Deletes the `metadata` row but preserves the `bundles` row. The unique constraint on `bundle_id` prevents reinsertion of the same bundle.

**Recovery protocol:**

Same three-phase protocol as SQLite:

1. `start_recovery()` populates an `unconfirmed` table with `ON CONFLICT DO NOTHING`.
2. `confirm_exists()` runs within an explicit transaction to atomically select the metadata and delete the unconfirmed entry.
3. `remove_unconfirmed()` deletes unconfirmed entries in batches using atomic CTEs. The `unconfirmed` table uses `ON DELETE CASCADE` from `metadata`, so deleting a metadata row automatically removes the corresponding unconfirmed entry.

**What happens if the system crashes during recovery:** Same as SQLite. `start_recovery` is idempotent, `confirm_exists` leaves unconfirmed entries for cleanup, and `remove_unconfirmed` uses atomic CTEs.

**Operator actions:** Standard PostgreSQL backup and recovery procedures apply. Hardy does not manage PostgreSQL replication or backups. Consider:

- Configuring PostgreSQL WAL archiving for point-in-time recovery.
- Running regular `pg_dump` backups.
- Using a connection pooler (e.g. PgBouncer) if running multiple Hardy instances against the same database.

## Cross-Store Failures

Since metadata and bundle data live in separate stores, three inconsistent states are possible after a crash:

**Metadata exists, bundle data missing** (crash after metadata insert but before bundle save completes):

- The recovery walk does not find the bundle data.
- The metadata entry stays unconfirmed.
- The cleanup phase deletes the orphaned metadata.

**Bundle data exists, metadata missing** (crash after bundle save but before metadata insert):

- The recovery walk finds the bundle data.
- `confirm_exists` finds no matching metadata.
- The bundle data is reported as orphaned and deleted.

**Both exist and are consistent:**

- `confirm_exists` succeeds, the entry is removed from the unconfirmed set, and normal operation resumes.

In all cases, no manual intervention is required. The recovery protocol handles every combination automatically.
