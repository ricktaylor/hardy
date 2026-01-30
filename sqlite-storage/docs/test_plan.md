# Test Plan: SQLite Metadata Storage

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (SQLite Backend) |
| **Module** | `sqlite-storage` |
| **Implements** | `hardy_bpa::storage::MetadataStorage` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-7), `DTN-LLR_v1.1` (Section 7.2) |
| **Parent Plan** | `hardy-bpa/src/storage_integration_test_plan.md` |
| **Test Suite ID** | PLAN-SQLITE-01 |

## 1. Introduction

This document details the testing strategy for the `sqlite-storage` crate. This crate provides a persistent implementation of the `MetadataStorage` trait using SQLite.

## 2. Requirements Mapping

The following requirements from **DTN-LLR_v1.1** are verified by the unit tests in this plan:

| LLR ID | Description |
| :--- | :--- |
| **7.2.2** | Configurable filesystem location for SQLite database. |

## 3. Test Coverage

The following suites from the parent plan are executed against `sqlite-storage`:

### Suite A: Basic CRUD Operations

*Objective: Verify INSERT, SELECT, UPDATE, and DELETE (Tombstone) logic.*

* **META-01**: Insert & Get
* **META-02**: Duplicate Insert (Constraint handling)
* **META-03**: Update (Replace)
* **META-04**: Tombstone
* **META-05**: Confirm Exists

### Suite B: Polling & Ordering

*Objective: Verify SQL `ORDER BY` and `LIMIT` clauses match the required priority logic.*

* **META-06**: Poll Waiting (FIFO)
* **META-07**: Poll Expiry (Ordered by time)
* **META-08**: Poll Pending (FIFO & Limit)
* **META-09**: Poll Pending (Exact Match)
* **META-10**: Poll Fragments

### Suite C: State Transitions

*Objective: Verify complex updates and transactions.*

* **META-11**: Reset Peer Queue
* **META-12**: Recovery (Startup scan)
* **META-13**: Remove Unconfirmed

## 4. Unit Test Cases

### 4.1 Implementation Logic (LLR 7.2.2)

*Objective: Verify SQL-specific logic, migrations, and robustness.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Migration Logic (SQL-01)** | Verify that the database schema is correctly created and upgraded. | `src/migrate.rs` | Empty DB / Old DB. | Schema created / Upgraded successfully. |
| **Concurrency (SQL-02)** | Verify that the connection pool handles concurrent reads/writes. | `src/storage.rs` | Concurrent async writes. | No `SQLITE_BUSY` errors. |
| **Persistence (SQL-03)** | Verify data survives a process restart. | `src/storage.rs` | Write, Close, Reopen, Read. | Data present. |
| **Migration Errors (SQL-04)** | Verify the migration logic detects tampering. | `src/migrate.rs` | Modified `schema_versions` table. | Error returned (Missing/Extra/Altered). |
| **Corrupt Data (SQL-05)** | Verify that malformed bundle data is handled gracefully. | `src/storage.rs` | Manually insert bad bytes. | `get()` returns None/Tombstones. |
| **Waiting Queue (SQL-06)** | Verify `waiting_queue` cache invalidation. | `src/storage.rs` | Update status from Waiting -> Delivered. | Bundle removed from `waiting_queue`. |

## 5. Execution Strategy

* **Specific Tests:** `cargo test -p sqlite-storage`
* **Generic Tests:** `cargo test --test storage_harness` (via `hardy-bpa` harness)
