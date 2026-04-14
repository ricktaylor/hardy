# PostgreSQL Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-postgres-storage` |
| **Standard** | — |
| **Test Plans** | [`PLAN-PG-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Date** | 2026-04-14 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `MetadataStorage` trait contract is verified by the shared storage harness (14 tests, all pass when a PostgreSQL instance is available). Recovery requirements are satisfied by the harness tests exercising `MetadataStorage::start_recovery()` (META-12), `confirm_exists()` (META-05), and `remove_unconfirmed()` (META-13), with the BPA's `storage/recover.rs` orchestrating these trait methods during restart.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 8.1 | Metadata storage | **Pass** | META-01..14 ([`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4) |
| 8.2 | Recovery after restart | **Pass** | META-05 (confirm_exists) + META-12 (start_recovery) + META-13 (remove_unconfirmed) |

## 2. Test Inventory

### Generic harness tests (via `tests/storage/`)

14 integration tests run against PostgreSQL via `storage_meta_tests_async!(postgres, ...)` plus a dedicated `meta_05_confirm_exists` recovery test. See [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §3.5 for registration details and §4 for test scenarios.

Each test creates an isolated database with a random name (`hardy_test_{uuid}`) and drops it on completion. All 14 pass. No failures or skips.

### Backend-specific tests

No unit tests exist within the crate. No commented-out stubs.

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-PG-01`](test_plan.md) (backend-specific) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite A (META-01..05) | CRUD + recovery confirmation | 5 | 5 | Complete |
| `PLAN-STORE-01` Suite B (META-06..10, 14) | Polling & ordering | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite C (META-11..13) | State transitions & bulk ops | 3 | 3 | Complete |
| `PLAN-PG-01` §4 (PG-01..04) | Backend-specific | 4 | 0 | Not started |
| **Total** | | **18** | **14** | **78%** |

## 4. Line Coverage

Line coverage is not measurable for this crate. All verification runs through the external `tests/storage/` harness, and tests additionally require a running PostgreSQL instance, so coverage instrumentation is not practical in CI.

## 5. Key Gaps

| Test ID | Scenario | Severity | Notes |
| :--- | :--- | :--- | :--- |
| PG-01 | Connection pooling under load | Medium | Important for production deployments |
| PG-02 | Migration logic (create + upgrade) | Medium | Critical for production upgrades |
| PG-03 | Concurrent writers (same bundle ID) | Low | PostgreSQL provides strong serialisation defaults |
| PG-04 | Connection timeout / unreachable DB | Low | Operational robustness |

## 6. Conclusion

14 integration tests verify the full `MetadataStorage` trait contract through the shared storage harness (78% of planned scenarios). All CRUD, polling, recovery, and state transition operations pass against a real PostgreSQL instance with per-test database isolation. 4 backend-specific scenarios covering connection pooling, migration, concurrent access, and connection failure remain unimplemented.
