# Recovery

Hardy is designed to recover gracefully from crashes, power failures, and unexpected restarts. This page explains what happens when the server restarts, how each storage backend handles failures, and what operator actions may be needed.

## Restart and Recovery

Hardy separates **metadata storage** (bundle state, timestamps, status) from **bundle data storage** (the raw bytes). These two stores can become inconsistent if the process crashes between writing to one and the other.

Recovery runs only when the server is started with `--recover-store` (`-r`). It is a three-phase protocol that brings the two stores back into sync:

1. **Mark** -- all metadata entries are marked as _unconfirmed_.
2. **Confirm** -- the bundle data store is walked. For each bundle found, the corresponding metadata entry is confirmed.
3. **Cleanup** -- any metadata entry still unconfirmed is deleted, because its bundle data is missing.

Recovery also picks up the bundles that were in progress when the server stopped. A bundle that was being processed is processed again. A bundle that was queued for a peer or a service, or part-way through a transfer or a delivery, goes back to waiting for a route or for its service. When recovery completes, every metadata entry has matching bundle data, and only then does the server start its CLAs and services. It logs `Starting store consistency check...` as it begins and `Store consistency check completed` when it ends.

This protocol is idempotent. If the server crashes during recovery itself, starting it again with `--recover-store` runs the protocol again from the beginning.

### When to Use `--recover-store`

Start the server with `--recover-store` after any stop that was not an orderly shutdown (SIGTERM or Ctrl+C): a crash, a kill, or a power loss. Use it too after restoring a store from a backup or editing one by hand.

Without it, the server starts on whatever the crash left in its stores:

- **Bundle data saved without its metadata** stays in the bundle store. It is never forwarded, and nothing deletes it.
- **Metadata whose data is gone** stays until the server next tries to forward or deliver the bundle, when it drops the bundle and reports it deleted, or until the bundle expires.
- **Bundles in progress** keep the state they were in. A bundle that was being processed waits until it expires. Peers are numbered afresh each time the server starts, so a bundle queued for a peer can be sent to a different one, or wait until it expires. A bundle part-way through a transfer or a delivery may never be retried, and does not expire.

A supervisor that restarts the server after a crash (a systemd unit, a Docker Compose `restart:` policy) starts it the same way every time, so a supervised server recovers from a crash only if `--recover-store` is in its command line. In Docker Compose, add `-r` to the service's `command:`.

Recovery reads every stored bundle, so it lengthens the start in proportion to the size of the store. With the local disk backend it also depends on the system clock being right: see the recovery walk below.

## Bundle Data Backends

### Local Disk

The local disk backend stores each bundle as a file on the filesystem.

**Save operation:**

With `fsync` enabled (the default), saving a bundle is a five-step process designed so that a crash at any point leaves the system in a recoverable state:

1. Claim the bundle's final name by creating a 0-byte placeholder under a random name. The create fails if a file already has that name, and the save tries the next name.
2. Write the bundle data to a `.tmp` file beside the placeholder.
3. Sync the file data to disk (`fsync`).
4. Rename the `.tmp` file over the placeholder. This is an atomic operation on POSIX filesystems.
5. Sync the directory holding the file to make the rename durable.

A save that fails part-way removes its placeholder and its `.tmp` file. With `fsync` disabled, the save is different: see the `fsync` option below.

**What happens if the system crashes:**

| Crash point | State on disk | What happens on restart |
|---|---|---|
| During steps 1-3 | A 0-byte placeholder, and perhaps a `.tmp` file, exist | The files are deleted the next time recovery runs |
| After step 4, before step 5 | File has its final name but the directory is not synced | On most modern journaling filesystems the file will be visible. In rare cases it may be lost. |
| After step 5 | File is fully durable | Nothing to do |

A crash during the first three steps loses that bundle's data, but no metadata refers to it yet: the server records a bundle only after its save has completed.

**Recovery walk:** When recovery runs, the store directory is walked. All `.tmp` files and 0-byte placeholders are deleted. Empty directories are removed. A timestamp fence ensures that files being written by concurrent operations are not accidentally deleted. Surviving files are reported to the metadata store for confirmation.

The fence compares each file's time with the system clock when the walk starts, and skips any file that is newer. If the clock has been set backwards since files were written, those files look new and are not reported: recovery removes their metadata entries and reports those bundles deleted, and leaves their data files on disk with no metadata. On a system with no battery-backed clock that starts at a fixed date, that is every bundle in the store. Make sure the clock is right before the server starts with `--recover-store`. Where `--recover-store` is in a supervisor's command line and the clock can be behind at boot, start the server only once the clock has been synchronised.

**The `fsync` option:**

- `fsync: true` (default): the save above. The file data is synced before the rename, and the directory after it. This is the safest option.
- `fsync: false`: the bundle is written straight to its final name through the file that claims it, with no `.tmp` file and no rename, and no sync calls are made. The OS may buffer writes. A crash can lose data that has not yet been flushed to disk, and can leave an empty or partly written file under a final name: recovery deletes an empty file, and deletes a file it cannot read as a bundle. Use this only when performance matters more than durability (e.g. in-memory tmpfs, or when bundles can be retransmitted).

**Delete:** A single `remove_file` call. If the file is already gone, the error is ignored.

**Operator actions:** Start the server with `--recover-store` after a crash, with the clock set correctly.

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

**Operator actions:** Start the server with `--recover-store` (`-r`) after a crash: see [When to Use `--recover-store`](#when-to-use-recover-store). `--upgrade-store` (`-u`) upgrades the database schema to the current format.

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

**Metadata exists, bundle data missing** (crash part-way through deleting a bundle, which removes the data before the metadata; or data lost from the bundle store):

- The recovery walk does not find the bundle data.
- The metadata entry stays unconfirmed.
- The cleanup phase deletes the orphaned metadata, and the bundle is reported deleted.

**Bundle data exists, metadata missing** (crash after bundle save but before metadata insert):

- The recovery walk finds the bundle data.
- `confirm_exists` finds no matching metadata.
- The bundle is processed again as if it had just been received, and the old copy of its data is deleted.

**Both exist and are consistent:**

- `confirm_exists` succeeds, the entry is removed from the unconfirmed set, and normal operation resumes.

Recovery handles every combination without further action, once the server is started with `--recover-store`.
