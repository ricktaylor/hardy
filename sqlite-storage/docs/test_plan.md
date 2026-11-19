# Test Plan: SQLite Metadata Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (SQLite Backend) |
| **Module** | `sqlite-storage` |
| **Implements** | `hardy_bpa::storage::MetadataStorage` |
| **Requirements Ref** | [REQ-7](../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [LLR 7.2.x](../../docs/requirements.md#sqlite-storage-72) |
| **Parent Plan** | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Test Suite ID** | PLAN-SQLITE-01 |
| **Version** | 1.5 |

## 1. Introduction

This document defines the backend-specific tests for the `sqlite-storage` crate. This crate provides a persistent implementation of the `MetadataStorage` trait using SQLite.

Trait-level contract testing (CRUD, polling, ordering, state transitions, recovery) is covered by the generic storage harness — see [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4–§5 for test scenarios and §6 for the coverage boundary. This plan covers only what falls outside that boundary.

## 2. Requirements Mapping

| LLR ID | Description | Verified By |
| :--- | :--- | :--- |
| **7.2.1** | Store/retrieve metadata | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) Suite A (META-01..05) |
| **7.2.2** | Configurable filesystem location for SQLite database | SQL-01 |

## 3. Generic Harness Coverage

This backend is registered in the storage harness with `storage_meta_tests!(sqlite, ...)` plus a dedicated `meta_05_confirm_exists` recovery test. The following suites run against SQLite:

- Suite A: Basic CRUD Operations (META-01..04, META-15)
- Suite B: Polling & Ordering (META-06..10, META-14)
- Suite C: State Transitions & Bulk Ops (META-05, META-11..13, META-16)

Persistence across restart (META-05) is explicitly tested — the harness inserts data, triggers recovery, and verifies the entry survives. This is not duplicated here.

## 4. Backend-Specific Test Cases

*Objective: Verify SQLite-specific behaviour not observable through the `MetadataStorage` trait interface.*

| Test ID | Scenario | Source | Procedure | Expected Result |
| :--- | :--- | :--- | :--- | :--- |
| **SQL-01** | **Configuration** | `config.rs` | 1. Create storage with custom `db_dir`.<br>2. Verify database file created at path. | File exists at configured location. |
| **SQL-02** | **Migration logic** | `migrate.rs` | 1. Create storage (empty DB).<br>2. Verify schema tables exist.<br>3. Reopen (simulate upgrade). | Schema created on first run; no-op on reopen. |
| **SQL-03** | **Migration errors** | `migrate.rs` | 1. Create storage.<br>2. Manually alter `schema_versions` table.<br>3. Reopen. | Error returned (missing/extra/altered migration detected). |
| **SQL-04** | **Concurrency (SQLITE_BUSY)** | `storage.rs` | 1. Spawn N concurrent async writers.<br>2. All insert different bundles. | No `SQLITE_BUSY` errors; all inserts succeed. |
| **SQL-05** | **Corrupt data handling** | `storage.rs` | 1. Insert bundle via trait.<br>2. Manually corrupt row bytes in DB.<br>3. Call `get()`. | Graceful error or tombstone, not panic. |
| **SQL-06** | **Waiting queue invalidation** | `storage.rs` | 1. Insert bundle (Status=`Waiting`).<br>2. Claim it to `Dispatching` via `swap_status()`.<br>3. Call `poll_waiting()`. | Bundle not returned (cache correctly invalidated). |
| **SQL-07** | **Batched write isolation** | `writer.rs` | 1. Hold the writer thread inside one write.<br>2. Queue three writes, the middle one failing after its first statement.<br>3. Release the writer. | The failing write reports its own error and leaves no trace; the other two commit. |
| **SQL-08** | **Batch commit failure** | `writer.rs` | 1. Hold the writer thread.<br>2. Queue a valid insert and a write that violates a deferred foreign key.<br>3. Release the writer. | Both writes report the batch's transaction failure; neither takes effect. |
| **SQL-09** | **Abandoned write** | `writer.rs` | 1. Hold the writer thread.<br>2. Queue a write, then drop its caller.<br>3. Release the writer. | The write is never run. |
| **SQL-10** | **Query-only readers** | `readers.rs` | 1. Route an `INSERT` through the reader pool. | Fails with `SQLITE_READONLY`. |
| **SQL-11** | **Background checkpoint** | `checkpoint.rs` | 1. Write past the cycle threshold.<br>2. Let the writer request a cycle.<br>3. Wait for the checkpoint thread. | The whole WAL is copied into the database file with no checkpoint on the writer. |
| **SQL-12** | **No automatic checkpoint** | `checkpoint.rs` | 1. Write 1200 pages with the cycle threshold out of reach. | Nothing is copied: SQLite's own checkpoint at 1000 pages is off. |
| **SQL-13** | **WAL restart** | `checkpoint.rs` | 1. Hold a reader's snapshot so the background cycle stops short.<br>2. Release it and pass the restart bound.<br>3. Write again. | The writer copies the remainder, and the next write restarts the WAL from its beginning. |
| **SQL-14** | **No writer checkpoint below the restart bound** | `checkpoint.rs` | 1. As SQL-13, with the restart bound out of reach. | The writer copies nothing; the remainder waits for the next cycle. |
| **SQL-15** | **WAL size limit** | `checkpoint.rs` | 1. Write past the limit.<br>2. Let the writer request a cycle and call it again. | The writer waits for the cycle and returns with the whole WAL copied. |
| **SQL-16** | **Database file sync** | `checkpoint.rs` | 1. Sync the database file through SQLite's own file handle. | Succeeds. |
| **SQL-17** | **Tombstones leave the indexes** | `storage.rs` | 1. Insert two bundles.<br>2. Tombstone one.<br>3. Count each secondary index's entries, and re-insert the tombstoned bundle. | Each index drops from two entries to one; the row stays and refuses the replay. |
| **SQL-18** | **Expiry poll plan** | `storage.rs` | 1. `EXPLAIN QUERY PLAN` the expiry poll's SQL. | A search of `idx_bundles_expiry`, with no table scan and no sort. |
| **SQL-19** | **Secondary index set** | `storage.rs` | 1. List the indexes on `bundles`. | Exactly the two live-row indexes, both partial. |
| **SQL-20** | **Batch loses its transaction** | `writer.rs` | 1. Cap the database's size.<br>2. Queue a small write, a write that fills the database, and another small write.<br>3. Release the writer. | All three report the batch's failure with `SQLITE_FULL`; no row is written; the writer's next write succeeds. |
| **SQL-21** | **Upgrade of an existing store** | `migrate.rs` | 1. Migrate a store with all but the newest migration (and, separately, with only the first).<br>2. Reopen with `upgrade`.<br>3. Reopen a store one behind without `upgrade`. | Every pending migration runs and is recorded, and the newest one's schema change is present; without `upgrade` the store is refused with `UpdateRequired` and left unchanged. |
| **SQL-22** | **Abort on a store thread's panic** | `thread.rs`, `writer.rs`, `readers.rs` | In a child process:<br>1. Panic on a worker thread.<br>2. Panic inside a write.<br>3. Panic inside a read. | Each child is killed by `SIGABRT`. A worker whose work returns is joined without aborting. |
| **SQL-23** | **Failed checkpoint** | `checkpoint.rs` | 1. Give a step the `SQLITE_BUSY` error.<br>2. In a child process, run a cycle on a connection whose database has no file to sync. | The busy step is deferred; the failed cycle aborts the child. |

## 5. Execution

```sh
# Backend-specific tests (when implemented)
cargo test -p hardy-sqlite-storage

# Generic harness (covers trait contract)
cargo test -p storage-tests
```
