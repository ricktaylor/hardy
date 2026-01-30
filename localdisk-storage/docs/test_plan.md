# Test Plan: Local Disk Bundle Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (Local Filesystem Backend) |
| **Module** | `localdisk-storage` |
| **Implements** | `hardy_bpa::storage::BundleStorage` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-7), `DTN-LLR_v1.1` (Section 7.1) |
| **Parent Plan** | `hardy-bpa/src/storage_integration_test_plan.md` |
| **Test Suite ID** | PLAN-LD-01 |

## 1. Introduction

This document details the testing strategy for the `localdisk-storage` crate. This crate provides a persistent implementation of the `BundleStorage` trait using the local filesystem, storing each bundle as a separate file.

## 2. Requirements Mapping

The following requirements from **DTN-LLR_v1.1** are verified by the unit tests in this plan:

| LLR ID | Description |
| :--- | :--- |
| **7.1.1** | Configurable location for Local Disk bundle storage. |
| **7.1.2** | Configurable maximum total size for Local Disk storage. |

## 3. Generic Test Coverage

The following suites from the parent plan (`PLAN-STORE-01`) are executed against `localdisk-storage`:

### Suite D: Payload Operations

*Objective: Verify the fundamental storage and retrieval of binary bundle data.*

* **BLOB-01**: Save & Load
* **BLOB-02**: Delete
* **BLOB-03**: Missing Load
* **BLOB-04**: Recovery Scan

## 4. Unit Test Cases

### 4.1 Implementation Logic (LLR 7.1.1)

*Objective: Verify robustness of the filesystem interactions and recovery logic.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Atomic Save (LD-01)** | Verifies the "save-to-temp, then rename" logic when `fsync` is enabled. | `src/storage.rs` | `save(data)` with `fsync=true`. | Data written to `.tmp`, synced, renamed. No partial files on failure. |
| **Recovery Logic (LD-02)** | Verifies the `recover()` function correctly handles a dirty storage directory. | `src/storage.rs` | Dir with valid bundles, `.tmp` files, empty dirs. | Returns valid bundles; cleans up garbage. |
| **Filesystem Structure (LD-03)** | Verifies that the `xx/yy/` two-level directory structure is created correctly. | `src/storage.rs` | `save()` multiple bundles. | Files distributed in subdirs; collisions handled. |
| **Mmap Feature (LD-04)** | Verify `load()` works with `mmap` enabled/disabled. | `src/storage.rs` | `load(path)` | Returns correct bytes regardless of feature flag. |
| **Persistence (LD-05)** | Verifies that saved data survives the `Storage` object being dropped and recreated. | `src/storage.rs` | `Storage::new`, `save`, drop, `Storage::new`, `load`. | Data persists across instance lifecycle. |

## 5. Execution Strategy

* **Specific Tests:** `cargo test -p localdisk-storage`
* **Generic Tests:** `cargo test --test storage_harness` (via `hardy-bpa` harness)
