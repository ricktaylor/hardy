# Test Plan: Local Disk Bundle Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (Local Filesystem Backend) |
| **Module** | `localdisk-storage` |
| **Implements** | `hardy_bpa::storage::BundleStorage` |
| **Requirements Ref** | [REQ-7](../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [LLR 7.1.x](../../docs/requirements.md#local-disk-storage-71) |
| **Parent Plan** | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Test Suite ID** | PLAN-LD-01 |
| **Version** | 1.5 |

## 1. Introduction

This document defines the backend-specific tests for the `localdisk-storage` crate. This crate provides a persistent implementation of the `BundleStorage` trait using the local filesystem, storing each bundle as a separate file in a two-level directory structure.

Trait-level contract testing (save, load, delete, recovery scan) is covered by the generic storage harness — see [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §5 for test scenarios and §6 for the coverage boundary. This plan covers only what falls outside that boundary.

## 2. Requirements Mapping

| LLR ID | Description | Verified By |
| :--- | :--- | :--- |
| **7.1.1** | Configurable location for Local Disk bundle storage | LD-01 |
| **7.1.2** | Withdrawn in requirements 1.3 (capacity moves to the BPA, LLR 7.3.7) | — |
| **7.3.8** | A failed or interrupted write never leaves a partial bundle treated as complete | LD-04, LD-05, LD-09 |
| **7.1** | Store/retrieve bundles | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) Suite D (BLOB-01..05) |
| **7.3** | Recovery after restart | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) BLOB-04 |

## 3. Generic Harness Coverage

This backend is registered in the storage harness with `storage_blob_tests!(localdisk, ...)`. The following suite runs against localdisk:

- Suite D: Payload Operations (BLOB-01..05)

## 4. Backend-Specific Test Cases

*Objective: Verify filesystem-specific behaviour not observable through the `BundleStorage` trait interface.*

| Test ID | Scenario | Source | Procedure | Expected Result |
| :--- | :--- | :--- | :--- | :--- |
| **LD-01** | **Configuration** | `config.rs` | 1. Create storage with custom `store_dir`.<br>2. Save a bundle.<br>3. Verify file created under configured path. | File exists at configured location. |
| **LD-02** | **Recovery cleanup** | `storage.rs` | 1. Create storage dir with valid bundles, `.tmp` files, zero-byte files, and empty subdirectories.<br>2. Call `recover()`. | Valid bundles returned; `.tmp` files, zero-byte files, and empty dirs cleaned up. |
| **LD-03** | **Filesystem structure** | `storage.rs` | 1. Save multiple bundles.<br>2. Inspect directory tree. | Files distributed in `xx/yy/` two-level subdirectories; filename collisions resolved without error. |
| **LD-04** | **Atomic save** | `storage.rs` | 1. Call `save(data)` with `fsync=true`.<br>2. Inspect the store directory. | The bundle's file is the only file in the store: no `.tmp` file or placeholder remains; the data round-trips. |
| **LD-05** | **Write failure handling** | `storage.rs` | 1. Make store directory read-only.<br>2. Call `save(data)`. | The directory's refusal (permission denied) is returned, not a panic. |
| **LD-06** | **Name claim** | `storage.rs` | 1. Save a bundle through the store at a fixed bucket and file id, with `fsync` on and off.<br>2. Save a second bundle at the same placement. | The second save takes the next file id; the first bundle's bytes are unchanged; no placeholder or `.tmp` file remains. |
| **LD-07** | **Unsynced save** | `storage.rs` | 1. Call `save(data)` with `fsync=false`.<br>2. Inspect the store directory. | The bundle's file is the only file in the store; the data round-trips. |
| **LD-08** | **Stale temporary file** | `storage.rs` | 1. Put a `.tmp` file beside a free name.<br>2. Save through the store at that name, with `fsync` on and off. | The save succeeds under that name in both modes. With `fsync` on the stale file is gone; with `fsync` off, which uses no temporary file, it is untouched. |
| **LD-09** | **Failed save leaves no placeholder** | `storage.rs` | 1. Put a directory where an atomic save's `.tmp` file belongs.<br>2. Save atomically at that name. | The save fails with the filesystem's refusal; the store holds no file afterwards. |
| **LD-10** | **Concurrent name claim** | `storage.rs` | 1. Start 16 saves from one placement behind a barrier, into a bucket that does not exist, with `fsync` on and off. | Each save gets a distinct name holding its own bytes; the names are the 16 consecutive ids; nothing else is in the store. |
| **LD-11** | **Unsynced save opens its name once** | `storage.rs` | 1. With `fsync=false`, put another file under the name between the save's claim and its write. | The other file's contents are unchanged: the save wrote through the handle that claimed the name. |
| **LD-12** | **Failed rename removes the temporary file** | `storage.rs` | 1. Put a directory under the claimed name, so the rename fails.<br>2. Save atomically. | The save fails with the filesystem's refusal; the store holds no file afterwards. |
| **LD-13** | **Claim reports a created bucket** | `storage.rs` | 1. Claim a name in a bucket that does not exist.<br>2. Claim again in the same bucket. | The first claim reports that it created the bucket; the second reports that it did not. |
| **LD-14** | **Bucket removed under a claim** | `storage.rs` | 1. Remove the bucket once, straight after the claim creates it. | The claim creates the bucket again and succeeds, reporting that it created it. |
| **LD-15** | **Bucket that keeps disappearing** | `storage.rs` | 1. Remove the bucket every time the claim creates it, up to the claim's limit. | The claim fails with the filesystem's `NotFound` after that many creations; no file is created. |

## 5. Execution

```sh
# Backend-specific tests
cargo test -p hardy-localdisk-storage

# Generic harness (covers trait contract)
cargo test -p storage-tests
```
