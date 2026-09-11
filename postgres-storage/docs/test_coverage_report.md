# PostgreSQL Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-postgres-storage` |
| **Crate version** | `0.2.0` |
| **Standard** | — |
| **Test Plans** | [`PLAN-PG-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `MetadataStorage` trait contract is verified by the shared storage harness (16 tests, all pass when a PostgreSQL instance is available). Recovery requirements are satisfied by the harness recovery tests exercising `start_recovery()`, `confirm_exists()` (META-05), and `remove_unconfirmed()` (META-13), with the BPA's `storage/recover.rs` orchestrating these trait methods during restart.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 8.1 | Metadata storage | **Pass** | META-01..11, META-13..17 ([`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4) |
| 8.2 | Recovery after restart | **Pass** | META-05 (confirm_exists) + META-13 (remove_unconfirmed) |

## 2. Test Inventory

### Generic harness tests (via `tests/storage/`)

16 integration tests run against PostgreSQL: 14 via `storage_meta_tests!(postgres, ...)` plus 2 via the recovery suite `storage_meta_recovery_tests!(postgres_recovery, ...)`. See [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §3.5 for registration details and §4 for test scenarios.

Each test creates an isolated database with a random name (`hardy_test_{uuid}`) and drops it on completion. All 16 pass. No failures or skips.

### Backend-specific tests

No unit tests planned — backend-specific scenarios would test `sqlx`/PostgreSQL behaviour rather than Hardy code. See [`PLAN-PG-01` §4](test_plan.md) for rationale.

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-PG-01`](test_plan.md) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite A (META-01..04, 15, 17) | CRUD & tombstone semantics | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite B (META-06..10, 14) | Polling & ordering | 6 | 6 | Complete |
| `PLAN-STORE-01` Suite C (META-05, 11, 13, 16) | State transitions & bulk ops | 4 | 4 | Complete |
| **Total** | | **16** | **16** | **100%** |

## 4. Line Coverage

Line coverage is not measurable for this crate. All verification runs through the external `tests/storage/` harness, and tests additionally require a running PostgreSQL instance, so coverage instrumentation is not practical in CI.

## 5. Conclusion

16 integration tests verify the full `MetadataStorage` trait contract through the shared storage harness (100% of planned scenarios). All CRUD, polling, recovery, and state transition operations pass against a real PostgreSQL instance with per-test database isolation. No backend-specific unit tests are planned — see [`PLAN-PG-01` §4](test_plan.md) for rationale.
