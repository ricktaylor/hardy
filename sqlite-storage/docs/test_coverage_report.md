# SQLite Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-sqlite-storage` |
| **Crate version** | `0.6.0` |
| **Standard** | — |
| **Test Plans** | [`PLAN-SQLITE-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `MetadataStorage` trait contract is verified by the shared storage harness (16 tests, all pass). Recovery requirements are satisfied by the harness recovery tests exercising `start_recovery()`, `confirm_exists()` (META-05), and `remove_unconfirmed()` (META-13), with the BPA's `storage/recover.rs` orchestrating these trait methods during restart.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 7.2 | Metadata storage | **Pass** | META-01..11, META-13..17 ([`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4) |
| 7.2.1 | Store/retrieve metadata | **Pass** | META-01 (insert and get), META-03 (update/replace), META-17 (replace never resurrects a tombstone) |
| 7.2.2 | Configurable database location | **Pass** | SQL-01 (`test_configuration_custom_db_dir`) |
| 7.3 | Recovery after restart | **Pass** | META-05 (confirm_exists) + META-13 (remove_unconfirmed) |

## 2. Test Inventory

### Generic harness tests (via `tests/storage/`)

16 integration tests run against SQLite: 14 via `storage_meta_tests!(sqlite, ...)` plus 2 via the recovery suite `storage_meta_recovery_tests!(sqlite_recovery, ...)`. See [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §3.5 for registration details and §4 for test scenarios.

All 16 pass. No failures or skips.

### Backend-specific tests (`src/migrate.rs`, `tests/storage.rs`)

10 unit tests covering all 6 planned scenarios (SQL-01..06).

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `test_migration_creates_schema` | SQL-02 | Fresh DB: schema + version record created |
| `test_migration_reopen_is_noop` | SQL-02 | Re-migration: no new rows added |
| `test_migration_upgrade_required` | SQL-02 | upgrade=false on fresh DB returns error |
| `test_migration_detects_missing_historic` | SQL-03 | Renamed version row detected as missing |
| `test_migration_detects_extra_historic` | SQL-03 | Inserted fake version row detected |
| `test_migration_detects_altered_historic` | SQL-03 | Corrupted hash detected |
| `test_configuration_custom_db_dir` | SQL-01 | DB file created at configured path |
| `test_concurrency_no_sqlite_busy` | SQL-04 | 10 writers and 10 readers, barrier-released on a multi-thread runtime, all succeed |
| `test_corrupt_data_does_not_panic` | SQL-05 | Corrupt blob: get() errors, confirm_exists() tombstones |
| `test_waiting_queue_invalidation` | SQL-06 | Status change clears waiting queue |

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-SQLITE-01`](test_plan.md) (backend-specific) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite A (META-01..04, 15, 17) | CRUD & tombstone semantics | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite B (META-06..10, 14) | Polling & ordering | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite C (META-05, 11, 13, 16) | State transitions & bulk ops | 4 | 4 | Complete |
| `PLAN-SQLITE-01` §4 (SQL-01..06) | Backend-specific | 6 | 6 | Complete (10 tests) |
| **Total** | | **22** | **22** | **100%** |

## 4. Line Coverage

> Current figures are generated — see the [coverage summary](../../docs/coverage_summary.md) (refreshed by `scripts/run_lcov.sh`) and the live coverage dashboards (CFLite fuzz coverage on gh-pages; CI-published coverage planned). The snapshot below is from the crate version in the header.

```
cargo llvm-cov test --package hardy-sqlite-storage --lcov --output-path lcov.info
lcov --summary lcov.info
```

```
  lines......: 71.8% (455 of 634 lines)
  functions..: 37.4% (76 of 203 functions)
```

Unit tests (10) exercise migration logic, configuration, concurrency, corrupt data handling, and waiting queue invalidation. The uncovered lines are in the `MetadataStorage` trait implementation (poll methods, recovery protocol) which are exercised by the generic storage harness (16 tests) — that harness runs in a separate crate and is not captured by `llvm-cov`. The function-coverage figure varies with monomorphisation counting and is not a meaningful measure here; line coverage is the reliable signal.

## 5. Conclusion

26 tests (16 integration + 10 unit) verify both the `MetadataStorage` trait contract and all backend-specific scenarios (100% of planned scenarios). All trait-level operations pass: CRUD, polling with FIFO ordering, exact-match filtering, peer queue reset, recovery protocol, and fragment handling. Backend-specific tests cover migration logic and tamper detection, concurrent writer and reader safety, corrupt data resilience, configuration, and waiting queue cache correctness.
