# SQLite Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-sqlite-storage` |
| **Standard** | — |
| **Test Plans** | [`PLAN-SQLITE-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Date** | 2026-04-14 (updated) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `MetadataStorage` trait contract is verified by the shared storage harness (14 tests, all pass). Recovery requirements are satisfied by the harness tests exercising `MetadataStorage::start_recovery()` (META-12), `confirm_exists()` (META-05), and `remove_unconfirmed()` (META-13), with the BPA's `storage/recover.rs` orchestrating these trait methods during restart.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 7.2 | Metadata storage | **Pass** | META-01..14 ([`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4) |
| 7.2.1 | Store/retrieve metadata | **Pass** | META-01 (insert and get), META-03 (update/replace) |
| 7.2.2 | Configurable database location | **Pass** | SQL-01 (`test_configuration_custom_db_dir`) |
| 7.3 | Recovery after restart | **Pass** | META-05 (confirm_exists) + META-12 (start_recovery) + META-13 (remove_unconfirmed) |

## 2. Test Inventory

### Generic harness tests (via `tests/storage/`)

14 integration tests run against SQLite via `storage_meta_tests!(sqlite, ...)` plus a dedicated `meta_05_confirm_exists` recovery test. See [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §3.5 for registration details and §4 for test scenarios.

All 14 pass. No failures or skips.

### Backend-specific tests (`migrate.rs`, `storage.rs`)

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
| `test_concurrency_no_sqlite_busy` | SQL-04 | 10 concurrent writers, all succeed |
| `test_corrupt_data_does_not_panic` | SQL-05 | Corrupt blob: get() errors, confirm_exists() tombstones |
| `test_waiting_queue_invalidation` | SQL-06 | Status change clears waiting queue |

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-SQLITE-01`](test_plan.md) (backend-specific) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite A (META-01..05) | CRUD + recovery confirmation | 5 | 5 | Complete |
| `PLAN-STORE-01` Suite B (META-06..10, 14) | Polling & ordering | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite C (META-11..13) | State transitions & bulk ops | 3 | 3 | Complete |
| `PLAN-SQLITE-01` §4 (SQL-01..06) | Backend-specific | 6 | 6 | Complete (10 tests) |
| **Total** | | **20** | **20** | **100%** |

## 4. Line Coverage

```
cargo llvm-cov test --package hardy-sqlite-storage --lcov --output-path lcov.info
lcov --summary lcov.info
```

```
  lines......: 70.9% (438 of 618 lines)
  functions..: 68.3% (71 of 104 functions)
```

Unit tests (10) exercise migration logic, configuration, concurrency, corrupt data handling, and waiting queue invalidation. The uncovered lines are in the `MetadataStorage` trait implementation (poll methods, recovery protocol) which are exercised by the generic storage harness in a separate crate.

Unit tests (10) exercise migration logic, configuration, concurrency, corrupt data handling, and waiting queue invalidation. The generic storage harness (14 tests) runs in a separate crate and is not captured by `llvm-cov`.

## 5. Conclusion

24 tests (14 integration + 10 unit) verify both the `MetadataStorage` trait contract and all backend-specific scenarios (100% of planned scenarios). All trait-level operations pass: CRUD, polling with FIFO ordering, exact-match filtering, peer queue reset, recovery protocol, and fragment handling. Backend-specific tests cover migration logic and tamper detection, concurrent writer safety, corrupt data resilience, configuration, and waiting queue cache correctness.
