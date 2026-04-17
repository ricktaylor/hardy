# Test Plan: PostgreSQL Metadata Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (PostgreSQL Backend) |
| **Module** | `postgres-storage` |
| **Implements** | `hardy_bpa::storage::MetadataStorage` |
| **Requirements Ref** | [REQ-8](../../docs/requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage), Part 4 ref 8.1, 8.2 |
| **Parent Plan** | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Test Suite ID** | PLAN-PG-01 |
| **Version** | 1.0 |

## 1. Introduction

This document defines the backend-specific tests for the `postgres-storage` crate. This crate provides a persistent implementation of the `MetadataStorage` trait using PostgreSQL, enabling shared metadata storage across multiple BPA instances.

Trait-level contract testing (CRUD, polling, ordering, state transitions, recovery) is covered by the generic storage harness — see [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4 for test scenarios and §6 for the coverage boundary. This plan covers only what falls outside that boundary.

## 2. Requirements Mapping

| Ref | Description | Verified By |
| :--- | :--- | :--- |
| **8.1** | Store additional metadata in a remote PostgreSQL database instance | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) Suite A–C (META-01..14) |
| **8.2** | Restart the system and recover state from a remote PostgreSQL instance | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) META-05, META-12, META-13 |

## 3. Generic Harness Coverage

This backend is registered in the storage harness with `storage_meta_tests_async!(postgres, ...)` plus a dedicated `meta_05_confirm_exists` recovery test. The following suites run against PostgreSQL:

- Suite A: Basic CRUD Operations (META-01..05)
- Suite B: Polling & Ordering (META-06..10, META-14)
- Suite C: State Transitions & Bulk Ops (META-11..13)

Requires `--features postgres` and a running PostgreSQL instance (default: `postgresql://hardy:hardy@localhost:5432`).

## 4. Backend-Specific Test Rationale

No unit tests are planned for this crate. The `MetadataStorage` trait contract is fully verified by the generic harness against a real PostgreSQL instance (14 tests, feature-gated behind `--features postgres`). The backend-specific scenarios listed below are inherently integration concerns — connection pooling, migration, concurrent access, and connection failure all depend on a live database and are effectively testing `sqlx` and PostgreSQL behaviour rather than Hardy code. Adding crate-level unit tests would duplicate the harness coverage or test third-party library semantics.

If backend-specific integration tests are needed in future (e.g. for schema upgrade verification across releases), they should be added to the storage harness as new feature-gated tests, reusing the existing per-test database isolation infrastructure.

## 5. Execution

```sh
# Generic harness (covers trait contract — requires running PostgreSQL)
TEST_POSTGRES_URL=postgresql://hardy:hardy@localhost:5432 \
  cargo test -p storage-tests --features postgres
```
