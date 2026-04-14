# Test Plan: PostgreSQL Metadata Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (PostgreSQL Backend) |
| **Module** | `postgres-storage` |
| **Implements** | `hardy_bpa::storage::MetadataStorage` |
| **Requirements Ref** | [REQ-8](../../docs/requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage), Part 4 ref 8.1, 8.2 |
| **Parent Plan** | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Test Suite ID** | PLAN-PG-01 |
| **Version** | 1.1 |

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

## 4. Backend-Specific Test Cases

*Objective: Verify PostgreSQL-specific behaviour not observable through the `MetadataStorage` trait interface.*

| Test ID | Scenario | Source | Procedure | Expected Result |
| :--- | :--- | :--- | :--- | :--- |
| **PG-01** | **Connection pooling** | `storage.rs` | 1. Spawn N concurrent async readers and writers.<br>2. All operate on different bundles. | All operations complete; no pool exhaustion or timeout errors. |
| **PG-02** | **Migration logic** | `status.rs` | 1. Create storage (empty DB).<br>2. Verify schema tables exist.<br>3. Reopen (simulate upgrade). | Schema created on first run; no-op on reopen. |
| **PG-03** | **Concurrent writers** | `storage.rs` | 1. Spawn two tasks inserting the same bundle ID concurrently. | Exactly one returns `true`; no deadlock or constraint violation panic. |
| **PG-04** | **Connection timeout** | `lib.rs` | 1. Configure with invalid connection string or stopped database.<br>2. Attempt to create storage. | Returns an error; does not panic or hang indefinitely. |

## 5. Execution

```sh
# Backend-specific tests (when implemented)
cargo test -p hardy-postgres-storage

# Generic harness (covers trait contract — requires running PostgreSQL)
TEST_POSTGRES_URL=postgresql://hardy:hardy@localhost:5432 \
  cargo test -p storage-tests --features postgres
```
