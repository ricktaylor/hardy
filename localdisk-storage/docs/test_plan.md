# Test Plan: Local Disk Bundle Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (Local Filesystem Backend) |
| **Module** | `localdisk-storage` |
| **Implements** | `hardy_bpa::storage::BundleStorage` |
| **Requirements Ref** | [REQ-7](../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [LLR 7.1.x](../../docs/requirements.md#local-disk-storage-71) |
| **Parent Plan** | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Test Suite ID** | PLAN-LD-01 |
| **Version** | 1.2 |

## 1. Introduction

This document defines the backend-specific tests for the `localdisk-storage` crate. This crate provides a persistent implementation of the `BundleStorage` trait using the local filesystem, storing each bundle as a separate file in a two-level directory structure.

Trait-level contract testing (save, load, delete, recovery scan) is covered by the generic storage harness — see [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §5 for test scenarios and §6 for the coverage boundary. This plan covers only what falls outside that boundary.

## 2. Requirements Mapping

| LLR ID | Description | Verified By |
| :--- | :--- | :--- |
| **7.1.1** | Configurable location for Local Disk bundle storage | LD-01 |
| **7.1.2** | Configurable maximum total size for Local Disk storage | LD-05 |
| **7.1** | Store/retrieve bundles | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) Suite D (BLOB-01..04) |
| **7.3** | Recovery after restart | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) BLOB-04 |

## 3. Generic Harness Coverage

This backend is registered in the storage harness with `storage_blob_tests!(localdisk, ...)`. The following suite runs against localdisk:

- Suite D: Payload Operations (BLOB-01..04)

## 4. Backend-Specific Test Cases

*Objective: Verify filesystem-specific behaviour not observable through the `BundleStorage` trait interface.*

| Test ID | Scenario | Source | Procedure | Expected Result |
| :--- | :--- | :--- | :--- | :--- |
| **LD-01** | **Configuration** | `config.rs` | 1. Create storage with custom `store_dir`.<br>2. Save a bundle.<br>3. Verify file created under configured path. | File exists at configured location. |
| **LD-02** | **Recovery cleanup** | `storage.rs` | 1. Create storage dir with valid bundles, `.tmp` files, zero-byte files, and empty subdirectories.<br>2. Call `recover()`. | Valid bundles returned; `.tmp` files, zero-byte files, and empty dirs cleaned up. |
| **LD-03** | **Filesystem structure** | `storage.rs` | 1. Save multiple bundles.<br>2. Inspect directory tree. | Files distributed in `xx/yy/` two-level subdirectories; filename collisions resolved without error. |
| **LD-04** | **Atomic save** | `storage.rs` | 1. Call `save(data)` with `fsync=true`.<br>2. Verify file written to `.tmp` then renamed. | No partial files visible; rename is atomic. |
| **LD-05** | **Write failure handling** | `storage.rs` | 1. Make store directory read-only.<br>2. Call `save(data)`. | Graceful error returned, not panic. |

## 5. Execution

```sh
# Backend-specific tests (when implemented)
cargo test -p hardy-localdisk-storage

# Generic harness (covers trait contract)
cargo test -p storage-tests
```
