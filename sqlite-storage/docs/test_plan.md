# Test Plan: SQLite Metadata Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (SQLite Backend) |
| **Module** | `sqlite-storage` |
| **Implements** | `hardy_bpa::storage::MetadataStorage` |
| **Requirements Ref** | [REQ-7](../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [LLR 7.2.x](../../docs/requirements.md#315-sqlite-storage-parent-req-7) |
| **Parent Plan** | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) |
| **Test Suite ID** | PLAN-SQLITE-01 |
| **Version** | 1.1 |

## 1. Introduction

This document defines the backend-specific tests for the `sqlite-storage` crate. This crate provides a persistent implementation of the `MetadataStorage` trait using SQLite.

Trait-level contract testing (CRUD, polling, ordering, state transitions, recovery) is covered by the generic storage harness — see [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) §4–§5 for test scenarios and §6 for the coverage boundary. This plan covers only what falls outside that boundary.

## 2. Requirements Mapping

| LLR ID | Description | Verified By |
| :--- | :--- | :--- |
| **7.2.1** | Store/retrieve metadata | [`PLAN-STORE-01`](../../tests/storage/docs/test_plan.md) Suite A (META-01..05) |
| **7.2.2** | Configurable filesystem location for SQLite database | SQL-01 |

## 3. Generic Harness Coverage

This backend is registered in the storage harness with `storage_meta_tests!(sqlite, ...)` plus a dedicated `meta_05_confirm_exists` recovery test. The following suites run against SQLite:

- Suite A: Basic CRUD Operations (META-01..05)
- Suite B: Polling & Ordering (META-06..10, META-14)
- Suite C: State Transitions & Bulk Ops (META-11..13)

Persistence across restart (META-05) is explicitly tested — the harness inserts data, triggers recovery, and verifies the entry survives. This is not duplicated here.

## 4. Backend-Specific Test Cases

*Objective: Verify SQLite-specific behaviour not observable through the `MetadataStorage` trait interface.*

| Test ID | Scenario | Source | Procedure | Expected Result |
| :--- | :--- | :--- | :--- | :--- |
| **SQL-01** | **Configuration** | `config.rs` | 1. Create storage with custom `db_dir`.<br>2. Verify database file created at path. | File exists at configured location. |
| **SQL-02** | **Migration logic** | `migrate.rs` | 1. Create storage (empty DB).<br>2. Verify schema tables exist.<br>3. Reopen (simulate upgrade). | Schema created on first run; no-op on reopen. |
| **SQL-03** | **Migration errors** | `migrate.rs` | 1. Create storage.<br>2. Manually alter `schema_versions` table.<br>3. Reopen. | Error returned (missing/extra/altered migration detected). |
| **SQL-04** | **Concurrency (SQLITE_BUSY)** | `storage.rs` | 1. Spawn N concurrent async writers.<br>2. All insert different bundles. | No `SQLITE_BUSY` errors; all inserts succeed. |
| **SQL-05** | **Corrupt data handling** | `storage.rs` | 1. Insert bundle via trait.<br>2. Manually corrupt row bytes in DB.<br>3. Call `get()`. | Graceful error or tombstone, not panic. |
| **SQL-06** | **Waiting queue invalidation** | `storage.rs` | 1. Insert bundle (Status=`Waiting`).<br>2. Update status to `Delivered` via `replace()`.<br>3. Call `poll_waiting()`. | Bundle not returned (cache correctly invalidated). |

## 5. Execution

```sh
# Backend-specific tests (when implemented)
cargo test -p hardy-sqlite-storage

# Generic harness (covers trait contract)
cargo test -p storage-tests
```
