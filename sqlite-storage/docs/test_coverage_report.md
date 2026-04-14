# SQLite Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-sqlite-storage` |
| **Standard** | — |
| **Test Plans** | [`PLAN-SQLITE-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Date** | 2026-04-14 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `MetadataStorage` trait contract is verified by the shared storage harness (14 tests, all pass). Recovery requirements are satisfied by the harness tests exercising `MetadataStorage::start_recovery()` (META-12), `confirm_exists()` (META-05), and `remove_unconfirmed()` (META-13), with the BPA's `storage/recover.rs` orchestrating these trait methods during restart.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 7.2 | Metadata storage | **Pass** | META-01..14 ([`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4) |
| 7.2.1 | Store/retrieve metadata | **Pass** | META-01 (insert and get), META-03 (update/replace) |
| 7.2.2 | Configurable database location | **Not tested** | SQL-01 ([`PLAN-SQLITE-01`](test_plan.md) §4) |
| 7.3 | Recovery after restart | **Pass** | META-05 (confirm_exists) + META-12 (start_recovery) + META-13 (remove_unconfirmed) |

## 2. Test Inventory

### Generic harness tests (via `tests/storage/`)

14 integration tests run against SQLite via `storage_meta_tests!(sqlite, ...)` plus a dedicated `meta_05_confirm_exists` recovery test. See [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §3.5 for registration details and §4 for test scenarios.

All 14 pass. No failures or skips.

### Backend-specific tests

No unit tests exist within the crate. 4 commented-out stubs are present (`migrate.rs:144–152`, `storage.rs:762–771`).

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-SQLITE-01`](test_plan.md) (backend-specific) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite A (META-01..05) | CRUD + recovery confirmation | 5 | 5 | Complete |
| `PLAN-STORE-01` Suite B (META-06..10, 14) | Polling & ordering | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite C (META-11..13) | State transitions & bulk ops | 3 | 3 | Complete |
| `PLAN-SQLITE-01` §4 (SQL-01..06) | Backend-specific | 6 | 0 | Not started |
| **Total** | | **20** | **14** | **70%** |

## 4. Line Coverage

Line coverage is not measurable for this crate. All verification runs through the external `tests/storage/` harness, which exercises the `MetadataStorage` trait via dynamic dispatch. `cargo llvm-cov --package hardy-sqlite-storage` reports 0% because the test binary lives in a separate crate.

## 5. Key Gaps

| Test ID | Scenario | Severity | Notes |
| :--- | :--- | :--- | :--- |
| SQL-01 | Configuration (custom `db_dir`) | Low | Partially covered by harness setup using `tempfile::TempDir` |
| SQL-02 | Migration logic (create + upgrade) | Medium | Critical for production upgrades |
| SQL-03 | Migration errors (tamper detection) | Low | Defence-in-depth |
| SQL-04 | Concurrency (SQLITE_BUSY) | Medium | Important for multi-task BPA |
| SQL-05 | Corrupt data handling | Low | Defence-in-depth |
| SQL-06 | Waiting queue cache invalidation | Low | Correctness of internal optimisation |

## 6. Conclusion

14 integration tests verify the full `MetadataStorage` trait contract through the shared storage harness (70% of planned scenarios). All trait-level operations pass: CRUD, polling with FIFO ordering, exact-match filtering, peer queue reset, recovery protocol, and fragment handling. 6 backend-specific scenarios covering configuration, migration, concurrency, and robustness remain unimplemented.
