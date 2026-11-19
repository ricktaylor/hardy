# Local Disk Storage Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-localdisk-storage` |
| **Crate version** | `0.6.0` |
| **Standard** | — |
| **Test Plans** | [`PLAN-LD-01`](test_plan.md), [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |

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

12 unit tests covering all 12 planned scenarios (LD-01..12).

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `test_configuration_custom_store_dir` | LD-01 | Bundle file created under configured store_dir |
| `test_recovery_cleanup` | LD-02 | .tmp files, zero-byte placeholders, empty dirs cleaned up; valid bundles recovered |
| `test_filesystem_structure` | LD-03 | Files distributed in xx/yy/ two-level hex directories |
| `test_atomic_save_no_tmp_residue` | LD-04 | fsync=true save leaves only the bundle's file; data round-trips correctly |
| `test_save_to_readonly_dir_returns_error` | LD-05 | Save to read-only directory returns permission denied, not panic |
| `test_save_never_overwrites_an_existing_name` | LD-06 | A save whose name is taken moves to the next id, in both fsync modes; the existing bundle is untouched |
| `test_unsynced_save_writes_final_name_only` | LD-07 | fsync=false save creates only the bundle's file; data round-trips correctly |
| `test_stale_tmp_file_does_not_block_a_save` | LD-08 | A leftover `.tmp` beside a free name does not fail a save; what remains differs by fsync mode, which pins the mode dispatch |
| `test_failed_atomic_save_leaves_nothing_behind` | LD-09 | A failed atomic save removes its placeholder |
| `test_concurrent_saves_claim_distinct_names` | LD-10 | 16 saves racing for one name each claim their own, in both fsync modes |
| `test_unsynced_save_never_reopens_its_name` | LD-11 | fsync=false save writes only through the claiming handle |
| `test_failed_rename_removes_the_temporary_file` | LD-12 | A failed rename removes the temporary file and the placeholder |

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-LD-01`](test_plan.md) (backend-specific) and [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) (trait contract).

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| `PLAN-STORE-01` Suite D (BLOB-01..04) | BundleStorage trait contract | 4 | 4 | Complete |
| `PLAN-LD-01` §4 (LD-01..12) | Backend-specific | 12 | 12 | Complete |
| **Total** | | **16** | **16** | **100%** |

## 4. Line Coverage

> Current figures are generated — see the [coverage summary](../../docs/coverage_summary.md) (refreshed by `scripts/run_lcov.sh`) and the live coverage dashboards (CFLite fuzz coverage on gh-pages; CI-published coverage planned). The snapshot below is from the crate version in the header.

```
cargo llvm-cov test --package hardy-localdisk-storage --lcov --output-path lcov.info
lcov --summary lcov.info
```

```
  lines......: 93.3% (471 of 505 lines)
  functions..: 85.9% (61 of 71 functions)
```

Unit tests only: the 12 tests exercise configuration, recovery cleanup, filesystem structure, both save modes, their name claim (sequential and concurrent), and each failure path of a save. The uncovered lines are the platform default store directory, the recovery walk's error and early-exit branches, the `load` and `delete` error arms, and the mmap load path, which is behind a feature the measurement does not enable. The generic storage harness runs in a separate crate and is not captured by `llvm-cov`.

## 5. Conclusion

16 tests (4 integration + 12 unit) verify both the `BundleStorage` trait contract and all backend-specific scenarios (100% of planned scenarios). Trait-level operations (save, load, delete, recovery scan) pass via the shared harness. Backend-specific tests cover configuration, recovery cleanup, filesystem layout, the atomic save with fsync and the unsynced save without it, the name claim in both modes and under concurrent saves, a stale temporary file, and the clean-up after each way a save can fail.
