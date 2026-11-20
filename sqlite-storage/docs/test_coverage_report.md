# SQLite Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-sqlite-storage` |
| **Crate version** | `0.6.0` |
| **Standard** | — |
| **Test Plans** | [`PLAN-SQLITE-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `MetadataStorage` trait contract is verified by the shared storage harness (16 tests, all pass). Recovery is verified end to end: the harness exercises `MetadataStorage::start_recovery()` (META-12), `confirm_exists()` (META-05) and the removal of unconfirmed records by `remove_unconfirmed()` (META-13). The BPA's `storage/recover.rs` orchestrates these trait methods during restart.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 7.2 | Metadata storage | **Pass** | META-01..16 ([`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4) |
| 7.2.1 | Store/retrieve metadata | **Pass** | META-01 (insert and get), META-03 (status update by compare-and-swap) |
| 7.2.2 | Configurable database location | **Pass** | SQL-01 (`test_configuration_custom_db_dir`) |
| 7.3 | Recovery after restart | **Pass** | META-05 (confirm_exists) + META-12 (start_recovery) + META-13 (remove_unconfirmed) |

## 2. Test Inventory

### Generic harness tests (via `tests/storage/`)

16 integration tests run against SQLite: 14 via `storage_meta_tests!(sqlite, ...)` plus the dedicated `meta_05_confirm_exists` and `meta_13_remove_unconfirmed` recovery tests. See [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §3.5 for registration details and §4 for test scenarios.

All 16 pass. No failures or skips.

### Backend-specific tests (`migrate.rs`, `storage.rs`, `writer.rs`, `reader_pool.rs`, `checkpoint.rs`, `thread.rs`)

43 unit tests. 35 cover the 23 planned scenarios (SQL-01..23); the other 8 pin the status encoding and the queue-poll query plans, which the plan does not list as scenarios.

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `test_configuration_custom_db_dir` | SQL-01 | DB file created at configured path |
| `test_migration_creates_schema` | SQL-02 | Fresh DB: schema + version record created |
| `test_migration_reopen_is_noop` | SQL-02 | Re-migration: no new rows added |
| `test_migration_upgrade_required` | SQL-02 | upgrade=false on fresh DB returns error |
| `test_migration_detects_missing_historic` | SQL-03 | Renamed version row detected as missing |
| `test_migration_detects_extra_historic` | SQL-03 | Inserted fake version row detected |
| `test_migration_detects_altered_historic` | SQL-03 | Corrupted hash detected |
| `test_concurrency_no_sqlite_busy` | SQL-04 | 10 concurrent writers, all succeed |
| `test_corrupt_data_does_not_panic` | SQL-05 | Corrupt blob: get() errors, confirm_exists() tombstones |
| `test_waiting_queue_invalidation` | SQL-06 | Status change clears waiting queue |
| `a_failing_write_rolls_back_alone` | SQL-07 | A failing write in a batch rolls back alone |
| `a_failed_commit_fails_its_whole_batch` | SQL-08 | A failed commit fails every write in the batch |
| `an_abandoned_write_is_not_run` | SQL-09 | A dropped caller's write is skipped |
| `a_reader_refuses_to_write` | SQL-10 | Reader connections are query-only |
| `a_cycle_copies_the_wal_in_the_background` | SQL-11 | The checkpoint thread copies the WAL |
| `the_wal_is_left_alone_below_the_cycle_threshold` | SQL-12 | SQLite's automatic checkpoint is off |
| `the_writer_finishes_a_cycle_past_the_restart_bound` | SQL-13 | The writer finishes a cycle and the WAL restarts |
| `the_writer_leaves_the_remainder_below_the_restart_bound` | SQL-14 | No writer checkpoint below the restart bound |
| `the_writer_waits_for_the_cycle_at_the_limit` | SQL-15 | The writer waits at the WAL size limit |
| `the_writer_carries_on_below_the_limit` | SQL-15 | The writer does not wait below the limit |
| `the_limit_is_noted_once_until_the_wal_shrinks` | SQL-15 | Reaching the limit warns once |
| `the_database_file_syncs_through_its_own_handle` | SQL-16 | The database file syncs through SQLite's handle |
| `test_tombstones_leave_the_secondary_indexes` | SQL-17 | A tombstone leaves both secondary indexes |
| `poll_expiry_plan_uses_the_live_row_index` | SQL-18 | The expiry poll is served by the live-row index |
| `bundles_has_only_the_live_row_indexes` | SQL-19 | No other secondary index exists |
| `a_write_that_loses_the_transaction_fails_its_whole_batch` | SQL-20 | A write that makes SQLite roll back the transaction fails its whole batch |
| `test_migration_applies_the_pending_migration` | SQL-21 | A store one migration behind is upgraded |
| `test_migration_applies_every_pending_migration` | SQL-21 | A store several migrations behind is upgraded |
| `test_migration_refuses_a_store_behind_without_upgrade` | SQL-21 | Without upgrade: `UpdateRequired`, store unchanged |
| `a_panicking_worker_aborts_the_process` | SQL-22 | A worker thread's panic aborts the process |
| `a_worker_that_returns_is_joined` | SQL-22 | A worker that returns is joined, with no abort |
| `a_panicking_write_aborts_the_process` | SQL-22 | A panic inside a write aborts the process |
| `a_panicking_read_aborts_the_process` | SQL-22 | A panic inside a read aborts the process |
| `a_busy_checkpoint_step_is_deferred` | SQL-23 | A step that lost a lock race is deferred |
| `a_failed_checkpoint_aborts_the_process` | SQL-23 | A failed checkpoint aborts the process |
| `test_journal_mode_is_wal` | — | The database is in WAL mode |
| `test_forward_ack_pending_roundtrip_and_reset` | — | `ForwardAckPending` round trip and peer reset |
| `test_swap_status_is_conditional` | — | `swap_status` applies only from the expected status |
| `test_tombstone_if_is_conditional` | — | `tombstone_if` applies only from the expected status |
| `test_dispatch_pending_round_trip_and_poll` | — | `DispatchPending` round trip and poll |
| `test_delivery_statuses_roundtrip_poll_and_reset` | — | Delivery statuses: round trip, poll and service reset |
| `poll_pending_plan_has_no_temp_btree` | — | The queue polls sort in index order |
| `status_codec_numbering_is_frozen` | — | The on-disk status numbering is unchanged |

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-SQLITE-01`](test_plan.md) (backend-specific) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite A (META-01..04, 15, 17) | CRUD & tombstone semantics | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite B (META-06..10, 14) | Polling & ordering | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite C (META-05, 11..13, 16) | State transitions & bulk ops | 5 | 5 | Complete |
| `PLAN-SQLITE-01` §4 (SQL-01..23) | Backend-specific | 23 | 23 | Complete (35 tests) |
| **Total** | | **40** | **40** | **100%** |

## 4. Line Coverage

> Current figures are generated — see the [coverage summary](../../docs/coverage_summary.md) (refreshed by `scripts/run_lcov.sh`) and the live coverage dashboards (CFLite fuzz coverage on gh-pages; CI-published coverage planned). The snapshot below is from the crate version in the header.

```
cargo llvm-cov test --package hardy-sqlite-storage --lcov --output-path lcov.info
lcov --summary lcov.info
```

```
  lines......: 90.0% (1514 of 1683 lines)
  functions..: 83.1% (374 of 450 functions)
```

The 43 unit tests exercise the writer thread, the checkpoint thread, the readers, migrations, the status encoding and the trait methods that have backend-specific tests. The lines left uncovered are mostly `MetadataStorage` methods exercised only by the generic storage harness (16 tests), which runs in a separate crate and is not captured by `llvm-cov`. The function-coverage figure varies with monomorphisation counting; line coverage is the reliable signal.

## 5. Conclusion

59 tests (16 integration + 43 unit) verify both the `MetadataStorage` trait contract and all backend-specific scenarios (100% of planned scenarios). All trait-level operations pass: CRUD, polling with FIFO ordering, exact-match filtering, peer queue reset, recovery protocol, and fragment handling. Backend-specific tests cover migration logic, tamper detection and the upgrade of an existing store; concurrent writer safety; batched-write isolation, commit failure and a batch that loses its transaction; the abort when a store thread panics or a checkpoint fails; query-only readers; background checkpoints and the WAL's restart and size limit; the live-row indexes; corrupt data resilience; configuration; and waiting queue cache correctness.
