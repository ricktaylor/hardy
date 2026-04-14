# Local Disk Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-localdisk-storage` |
| **Standard** | — |
| **Test Plans** | [`PLAN-LD-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Date** | 2026-04-14 (updated) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `BundleStorage` trait contract is verified by the shared storage harness (4 tests, all pass). Recovery requirements are satisfied by the harness test exercising `BundleStorage::recover()` (BLOB-04) and the backend-specific recovery cleanup test (LD-02).

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 7.1 | Bundle storage | **Pass** | BLOB-01..04 ([`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §5) |
| 7.1.1 | Configurable storage location | **Pass** | LD-01 (`test_configuration_custom_store_dir`) |
| 7.1.2 | Configurable maximum size | **Not tested** | Not planned (enforcement is in BPA, not backend) |
| 7.3 | Recovery after restart | **Pass** | BLOB-04 (recovery scan) + LD-02 (cleanup of .tmp, zero-byte, empty dirs) |

## 2. Test Inventory

### Generic harness tests (via `tests/storage/`)

4 integration tests run against localdisk via `storage_blob_tests!(localdisk, ...)`. See [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §3.5 for registration details and §5 for test scenarios.

All 4 pass. No failures or skips.

### Backend-specific tests (`storage.rs`)

5 unit tests covering all 5 planned scenarios (LD-01..05).

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `test_configuration_custom_store_dir` | LD-01 | Bundle file created under configured store_dir |
| `test_recovery_cleanup` | LD-02 | .tmp files, zero-byte placeholders, empty dirs cleaned up; valid bundles recovered |
| `test_filesystem_structure` | LD-03 | Files distributed in xx/yy/ two-level hex directories |
| `test_atomic_save_no_tmp_residue` | LD-04 | fsync=true save leaves no .tmp files; data round-trips correctly |
| `test_save_to_readonly_dir_returns_error` | LD-05 | Save to read-only directory returns Err, not panic |

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-LD-01`](test_plan.md) (backend-specific) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite D (BLOB-01..04) | BundleStorage trait contract | 4 | 4 | Complete |
| `PLAN-LD-01` §4 (LD-01..05) | Backend-specific | 5 | 5 | Complete |
| **Total** | | **9** | **9** | **100%** |

## 4. Line Coverage

```
cargo llvm-cov test --package hardy-localdisk-storage --lcov --output-path lcov.info --html
lcov --summary lcov.info
```

Unit tests (5) exercise configuration, recovery cleanup, filesystem structure, atomic save, and write failure handling. The generic storage harness (4 tests) runs in a separate crate and is not captured by `llvm-cov`.

## 5. Conclusion

9 tests (4 integration + 5 unit) verify both the `BundleStorage` trait contract and all backend-specific scenarios (100% of planned scenarios). Trait-level operations (save, load, delete, recovery scan) pass via the shared harness. Backend-specific tests cover configuration, recovery cleanup, filesystem layout, atomic save with fsync, and error handling on write failure.
