# Test Plan: S3 Bundle Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (S3 Backend) |
| **Module** | `s3-storage` |
| **Implements** | `hardy_bpa::storage::BundleStorage` |
| **Requirements Ref** | [REQ-9](../../docs/requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage), Part 4 ref 9.1, 9.2, [LLR 9.1.x](../../docs/requirements.md#316-s3-storage-parent-req-9) |
| **Parent Plan** | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Test Suite ID** | PLAN-S3-01 |
| **Version** | 1.1 |

## 1. Introduction

This document defines the backend-specific tests for the `s3-storage` crate. This crate provides a persistent implementation of the `BundleStorage` trait using the Amazon S3 API, storing each bundle as a separate object.

Trait-level contract testing (save, load, delete, recovery scan) is covered by the generic storage harness — see [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §5 for test scenarios and §6 for the coverage boundary. This plan covers only what falls outside that boundary.

## 2. Requirements Mapping

| Ref | Description | Verified By |
| :--- | :--- | :--- |
| **9.1** | Store bundles on a remote system supporting the Amazon S3 API | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) Suite D (BLOB-01..04) |
| **9.1.1** | Configurable location and access credentials for the S3 instance | S3-01 |
| **9.1.2** | Configurable maximum total for all bundle data stored on S3 | Not tested |
| **9.1.4** | Use common S3 APIs, avoiding provider-specific extensions | By design (`aws-sdk-s3`) |
| **9.2** | Restart the system and recover state from S3 | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) BLOB-04 |

## 3. Generic Harness Coverage

This backend is registered in the storage harness with `storage_blob_tests_async!(s3, ...)`. The following suite runs against S3:

- Suite D: Payload Operations (BLOB-01..04)

Requires `--features s3` and a running S3-compatible endpoint (default: MinIO at `http://localhost:9000`, bucket `hardy-test`).

## 4. Backend-Specific Test Cases

*Objective: Verify S3-specific behaviour not observable through the `BundleStorage` trait interface.*

| Test ID | Scenario | Source | Procedure | Expected Result |
| :--- | :--- | :--- | :--- | :--- |
| **S3-01** | **Configuration** | `config.rs` | 1. Create storage with custom endpoint, bucket, prefix, and region.<br>2. Save a bundle.<br>3. Verify object exists at expected key. | Object created under configured bucket/prefix. |
| **S3-02** | **Credential expiry** | `storage.rs` | 1. Configure with valid credentials.<br>2. Revoke or expire credentials mid-session.<br>3. Attempt `save()`. | Returns an error; does not panic. |
| **S3-03** | **Prefix isolation** | `storage.rs` | 1. Save objects under prefix A.<br>2. Save objects under prefix B.<br>3. Call `recover()` with prefix A. | Returns only prefix A objects. |
| **S3-04** | **Large object multipart** | `storage.rs` | 1. Call `save()` with payload exceeding multipart threshold. | Upload completes using multipart API. |

## 5. Execution

```sh
# Backend-specific tests (when implemented)
cargo test -p hardy-s3-storage

# Generic harness (covers trait contract — requires running S3/MinIO)
TEST_S3_ENDPOINT=http://localhost:9000 \
AWS_ACCESS_KEY_ID=minioadmin \
AWS_SECRET_ACCESS_KEY=minioadmin \
  cargo test -p storage-tests --features s3
```
