# Local Disk Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-localdisk-storage` |
| **Standard** | — |
| **Test Plans** | [`PLAN-LD-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Date** | 2026-04-14 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `BundleStorage` trait contract is verified by the shared storage harness (4 tests, all pass). Recovery requirements are satisfied by the harness test exercising `BundleStorage::recover()` (BLOB-04), with the BPA's `storage/recover.rs` orchestrating trait methods during restart.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 7.1 | Bundle storage | **Pass** | BLOB-01..04 ([`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §5) |
| 7.1.1 | Configurable storage location | **Not tested** | LD-01 ([`PLAN-LD-01`](test_plan.md) §4) |
| 7.1.2 | Configurable maximum size | **Not tested** | LD-05 ([`PLAN-LD-01`](test_plan.md) §4) |
| 7.3 | Recovery after restart | **Pass** | BLOB-04 (recovery scan) |

## 2. Test Inventory

### Generic harness tests (via `tests/storage/`)

4 integration tests run against localdisk via `storage_blob_tests!(localdisk, ...)`. See [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §3.5 for registration details and §5 for test scenarios.

All 4 pass. No failures or skips.

### Backend-specific tests

No unit tests exist within the crate. 2 commented-out stubs are present (`storage.rs:337–347`).

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-LD-01`](test_plan.md) (backend-specific) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite D (BLOB-01..04) | BundleStorage trait contract | 4 | 4 | Complete |
| `PLAN-LD-01` §4 (LD-01..05) | Backend-specific | 5 | 0 | Not started |
| **Total** | | **9** | **4** | **44%** |

## 4. Line Coverage

Line coverage is not measurable for this crate. All verification runs through the external `tests/storage/` harness, which exercises the `BundleStorage` trait via dynamic dispatch. `cargo llvm-cov --package hardy-localdisk-storage` reports 0% because the test binary lives in a separate crate.

## 5. Key Gaps

| Test ID | Scenario | Severity | Notes |
| :--- | :--- | :--- | :--- |
| LD-01 | Configuration (custom `store_dir`) | Low | Partially covered by harness setup using `tempfile::TempDir` |
| LD-02 | Recovery cleanup (.tmp files, empty dirs) | Low | Defence-in-depth; commented-out stub exists |
| LD-03 | Filesystem structure (xx/yy/ layout) | Low | Internal implementation detail; commented-out stub exists |
| LD-04 | Atomic save (write-to-tmp, rename, fsync) | Low | Correctness relies on OS rename semantics |
| LD-05 | Disk full handling | Low | Operational robustness |

## 6. Conclusion

4 integration tests verify the `BundleStorage` trait contract through the shared storage harness (44% of planned scenarios). All trait-level operations (save, load, delete, recovery scan) pass. 5 backend-specific scenarios covering configuration, recovery cleanup, filesystem layout, atomicity, and disk-full handling remain unimplemented.
