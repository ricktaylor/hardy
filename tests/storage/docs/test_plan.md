# Test Plan: Storage Integration (Metadata & Bundles)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (Metadata & Bundles) |
| **Package** | `storage-tests` |
| **Interfaces** | `crate::storage::MetadataStorage`, `crate::storage::BundleStorage` |
| **Requirements Ref** | [REQ-7](../../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [REQ-8](../../../docs/requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage), [REQ-9](../../../docs/requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage) |
| **Test Suite ID** | PLAN-STORE-01 |
| **Version** | 1.1 |
| **Status** | DRAFT |

## 1. Introduction

This document defines the integration testing strategy and test scenarios for the persistence layer of the BPA. It covers two distinct traits:

1. **`MetadataStorage`**: Stores structured bundle state (status, routing flags, timestamps).
2. **`BundleStorage`**: Stores opaque binary bundle payloads (BLOBs).

The `storage-tests` crate is a **generic integration harness** that verifies every storage backend against the same trait-level contract. It exists so that:

- Backend implementations are tested uniformly — a new backend is verified by registering it with the harness, not by writing a new test suite.
- Trait contract violations are caught at the integration boundary, not in BPA pipeline tests where the root cause is harder to isolate.
- Backend-specific tests (e.g. SQLite WAL mode, localdisk directory layout, S3 eventual consistency) remain separate and focused, because the generic contract is already covered here.

## 2. Requirements Mapping

| ID | Requirement | Test Coverage |
| :--- | :--- | :--- |
| [**REQ-7**](../../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage) | Support for local filesystem (SQLite/Local Disk). | Verified by running suite against `sqlite-storage`, `localdisk-storage`. |
| [**REQ-8**](../../../docs/requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage) | Support for PostgreSQL. | Verified by running suite against `postgres-storage`. |
| [**REQ-9**](../../../docs/requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage) | Support for S3 (Bundle Storage). | Verified by running suite against `s3-storage`. |
| [**7.2.1**](../../../docs/requirements.md#sqlite-storage-72) | Store/Retrieve metadata. | Covered by **Suite A (Metadata CRUD)**. |
| [**7.1.1**](../../../docs/requirements.md#local-disk-storage-71) | Store/Retrieve payloads. | Covered by **Suite D (Bundle CRUD)**. |
| [**7.1.3**](../../../docs/requirements.md#local-disk-storage-71) | Configurable discard policy. | Covered by **Suite B (Polling)**. |

## 3. Harness Architecture

### 3.1 Trait-Level Suites

The harness defines two test suites, one per storage trait:

| Suite | Trait | Source | Tests |
| :--- | :--- | :--- | :--- |
| Metadata | `MetadataStorage` | `src/metadata_suite.rs` | 14 (`meta_01` .. `meta_14`) |
| Bundle | `BundleStorage` | `src/bundle_suite.rs` | 4 (`blob_01` .. `blob_04`) |

Each test function takes an `Arc<dyn MetadataStorage>` or `Arc<dyn BundleStorage>` — the suite has no knowledge of which backend it is testing.

### 3.2 Backend Registration

Backends are registered in `tests/storage_harness.rs` using macros that generate a `#[tokio::test]` function per suite test:

| Macro | Setup signature | Use case |
| :--- | :--- | :--- |
| `storage_meta_tests!` | `fn() -> (Guard, Arc<dyn MetadataStorage>)` | Sync setup (Memory, SQLite) |
| `storage_blob_tests!` | `fn() -> (Guard, Arc<dyn BundleStorage>)` | Sync setup (Memory, Local disk) |
| `storage_meta_tests_async!` | `async fn() -> (Guard, Arc<dyn MetadataStorage>)` | Async setup (PostgreSQL) |
| `storage_blob_tests_async!` | `async fn() -> (Guard, Arc<dyn BundleStorage>)` | Async setup (S3) |

The `Guard` type is returned by the setup function and dropped when the test completes. This provides RAII cleanup regardless of test outcome.

### 3.3 Test Isolation

Each backend setup function creates an isolated environment so tests can run in parallel without interference:

| Backend | Isolation mechanism | Cleanup |
| :--- | :--- | :--- |
| Memory | Fresh in-memory instance | Dropped with test |
| SQLite | `tempfile::TempDir` — unique directory per test | `TempDir::drop()` deletes directory |
| Local disk | `tempfile::TempDir` — unique directory per test | `TempDir::drop()` deletes directory |
| PostgreSQL | Random database name (`hardy_test_{uuid}`) per test | `PostgresTestGuard::drop()` runs `DROP DATABASE ... (FORCE)` |
| S3/MinIO | Unique key prefix (`test-{uuid}`) within shared bucket | Prefix-scoped — no cross-test interference |

### 3.4 Feature Gating

Backends requiring external infrastructure are gated behind Cargo features to keep the default test run dependency-free:

| Feature | Backends enabled | Infrastructure required |
| :--- | :--- | :--- |
| *(default)* | Memory, SQLite, Local disk | None |
| `postgres` | + PostgreSQL | PostgreSQL 13+ (see `compose.storage-tests.yml`) |
| `s3` | + S3/MinIO | MinIO or AWS S3 (see `compose.storage-tests.yml`) |

### 3.5 Registered Backends

| Backend | Suite | Setup | Feature flag |
| :--- | :--- | :--- | :--- |
| Memory | MetadataStorage | `memory_meta_setup` | default |
| SQLite | MetadataStorage + META-05 | `sqlite_meta_setup` | default |
| PostgreSQL | MetadataStorage + META-05 | `postgres_meta_setup` (async) | `postgres` |
| Memory | BundleStorage | `memory_blob_setup` | default |
| Local disk | BundleStorage | `localdisk_blob_setup` | default |
| S3/MinIO | BundleStorage | `s3_blob_setup` (async) | `s3` |

## 4. Metadata Storage Suites

### Suite A: Basic CRUD Operations

*Objective: Verify the fundamental lifecycle of a bundle's metadata.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **META-01** | **Insert & Get** | 1. Create a random `Bundle`.<br>2. Call `insert()`.<br>3. Call `get()` with the ID. | 1. `insert` returns `true`.<br>2. `get` returns `Some(bundle)`.<br>3. Fields match original. |
| **META-02** | **Duplicate Insert** | 1. Insert a bundle.<br>2. Insert the same bundle again. | 1. First `insert` returns `true`.<br>2. Second `insert` returns `false`. |
| **META-03** | **Update (Replace)** | 1. Insert a bundle (Status=`Waiting`).<br>2. Modify status to `Dispatching`.<br>3. Call `replace()`.<br>4. Call `get()`. | 1. `replace` returns `Ok`.<br>2. `get` returns bundle with `Dispatching` status. |
| **META-04** | **Tombstone** | 1. Insert a bundle.<br>2. Call `tombstone()`.<br>3. Call `get()`.<br>4. Call `insert()` again. | 1. `tombstone` returns `Ok`.<br>2. `get` returns `None`.<br>3. `insert` returns `false` (prevents resurrection). |

### Suite B: Polling & Ordering

*Objective: Verify that the storage engine correctly indexes and retrieves bundles based on time and status.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **META-06** | **Poll Waiting (FIFO)** | 1. Insert Bundle A (Received T=100, Status=Waiting).<br>2. Insert Bundle B (Received T=200, Status=Waiting).<br>3. Call `poll_waiting()`. | 1. Returns Bundle A, then Bundle B (Ordered by Received Time). |
| **META-07** | **Poll Expiry** | 1. Insert Bundle A (Expiry T=500, Status=`Waiting`).<br>2. Insert Bundle B (Expiry T=300, Status=`Waiting`).<br>3. Insert Bundle C (Expiry T=100, Status=`New`).<br>4. Call `poll_expiry(limit=10)`.<br>5. Call `poll_expiry(limit=1)`. | 1. Step 4 returns Bundle B, then Bundle A (Ordered by Expiry Time).<br>2. Bundle C excluded (`New` status filtered).<br>3. Step 5 returns Bundle B only (limit respected). |
| **META-08** | **Poll Pending (FIFO & Limit)** | 1. Insert A (Status=X, T=100).<br>2. Insert B (Status=X, T=200).<br>3. Call `poll_pending(X, limit=1)`.<br>4. Call `poll_pending(X, limit=2)`. | 1. First call returns A only.<br>2. Second call returns A, then B (Strict FIFO). |
| **META-09** | **Poll Pending (Exact Match)** | 1. Insert A (Status=`ForwardPending { peer: 1, queue: Some(0) }`).<br>2. Insert B (Status=`ForwardPending { peer: 2, queue: Some(0) }`).<br>3. Insert C (Status=`ForwardPending { peer: 1, queue: Some(1) }`).<br>4. Call `poll_pending(ForwardPending { peer: 1, queue: Some(0) })`. | 1. Returns A only.<br>2. Does not return B (different `peer`) or C (different `queue`).<br>3. Verifies all enum fields participate in matching. |
| **META-10** | **Poll Fragments** | 1. Insert Bundle A (Status=`AduFragment { source: S, timestamp: T }`, `fragment_info.offset`=0).<br>2. Insert Bundle B (Status=`AduFragment { source: S, timestamp: T }`, `fragment_info.offset`=100).<br>3. Call `poll_adu_fragments(AduFragment { source: S, timestamp: T })`. | 1. Returns Bundle A, then Bundle B (Ordered by `fragment_info.offset` from bundle ID). |
| **META-14** | **Poll Service Waiting (FIFO & Filtering)** | 1. Insert Bundle A1 (Status=`WaitingForService { service: S1 }`, Received T=200).<br>2. Insert Bundle B1 (Status=`WaitingForService { service: S2 }`, Received T=150).<br>3. Insert Bundle A2 (Status=`WaitingForService { service: S1 }`, Received T=100).<br>4. Call `poll_service_waiting(S1)`.<br>5. Call `poll_service_waiting(S2)`. | 1. Step 4 returns A2, then A1 (FIFO by Received Time).<br>2. B1 excluded (different service).<br>3. Step 5 returns B1 only. |

### Suite C: State Transitions & Bulk Ops

*Objective: Verify complex state management operations required by the BPA.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **META-05** | **Confirm Exists (Recovery)** | _Persistent backends only._<br>1. Insert bundle A.<br>2. Call `start_recovery()` (marks A unconfirmed).<br>3. Call `confirm_exists(A)`.<br>4. Call `confirm_exists(B)` (never inserted).<br>5. Call `remove_unconfirmed(tx)`. | 1. Step 3 returns `Some(metadata)`.<br>2. Step 4 returns `None`.<br>3. Step 5 removes nothing (A was confirmed).<br>4. `get(A)` still returns `Some`. |
| **META-11** | **Reset Peer Queue** | 1. Insert Bundle A (Status=`ForwardPending { peer: 100, queue: Some(0) }`).<br>2. Insert Bundle B (Status=`ForwardPending { peer: 200, queue: Some(0) }`).<br>3. Call `reset_peer_queue(100)`. | 1. `reset_peer_queue` returns `true`.<br>2. Bundle A status becomes `Waiting`.<br>3. Bundle B status remains `ForwardPending`. |
| **META-12** | **Recovery** | 1. Call `start_recovery()`. | 1. Returns `()` (No panic/error). |
| **META-13** | **Remove Unconfirmed** | 1. Insert Bundle A.<br>2. Call `remove_unconfirmed(tx)`. | 1. Returns `Ok`.<br>2. `tx` receives bundles (if implementation supports unconfirmed state). |

## 5. Bundle Storage Suites

### Suite D: Payload Operations

*Objective: Verify the storage and retrieval of binary bundle data.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **BLOB-01** | **Save & Load** | 1. Generate random bytes (1KB).<br>2. Call `save(bytes)`.<br>3. Call `load(name)`. | 1. `save` returns a storage name (string).<br>2. `load` returns `Some(bytes)`.<br>3. Bytes match exactly. |
| **BLOB-02** | **Delete** | 1. Save bytes.<br>2. Call `delete(name)`.<br>3. Call `load(name)`. | 1. `delete` returns `Ok`.<br>2. `load` returns `None`. |
| **BLOB-03** | **Missing Load** | 1. Call `load("non-existent")`. | 1. Returns `Ok(None)` (Not an error). |
| **BLOB-04** | **Recovery Scan** | 1. Save Blob A.<br>2. Save Blob B.<br>3. Call `recover(tx)`. | 1. `tx` receives entries for A and B.<br>2. Entries contain correct storage name and timestamp (`RecoveryResponse = (Arc<str>, OffsetDateTime)`). |

## 6. Coverage Boundary

This section defines the boundary between generic harness coverage and backend-specific testing responsibility.

### 6.1 Covered by Harness (do not duplicate)

| Area | Test IDs | What is verified |
| :--- | :--- | :--- |
| CRUD lifecycle | META-01..04, BLOB-01..03 | Insert, get, update, tombstone, save, load, delete |
| Polling & ordering | META-06..10, META-14 | FIFO ordering, expiry filtering, pending limits, peer matching, fragment ordering, service filtering |
| State transitions | META-05, META-11..13 | Recovery confirmation, peer queue reset, recovery replay, unconfirmed cleanup |
| Recovery scan | BLOB-04 | Discovers all stored bundles on restart |

### 6.2 NOT covered — backend-specific responsibility

Each backend crate's `docs/TODO.md` tracks its own test gaps. These are areas where behaviour depends on the storage engine, not the trait contract:

| Area | Examples | Why not generic |
| :--- | :--- | :--- |
| Configuration | Default paths, TOML parsing, env overrides | Each backend has different config fields |
| Storage layout | Localdisk `xx/yy/` directories, S3 key structure | Implementation detail not visible through trait |
| Failure modes | Disk full, SQLITE_BUSY, S3 eventual consistency, WAL corruption | Error behaviour is engine-specific |
| Atomicity guarantees | Localdisk write-to-tmp-then-rename, PostgreSQL transactions | Implementation detail |
| Performance | Large dataset query time, concurrent writer throughput | Engine-dependent |
| Recovery edge cases | Localdisk `.tmp` cleanup, empty directory pruning | Cleanup logic is backend-specific |
| Schema management | SQLite migrations, PostgreSQL schema upgrades | Only applicable to SQL backends |

## 7. Adding a Backend

1. Add the crate dependency to `Cargo.toml` (optionally feature-gated).
2. Add a setup function in `src/lib.rs` returning `(Guard, Arc<dyn Trait>)`.
3. Register it in `tests/storage_harness.rs` using the appropriate macro.
4. If the backend is persistent, add a separate `meta_05_confirm_exists` test (not included in the macro suites — it only applies to backends that survive process restart).
5. Create backend-specific tests in the backend's own crate for areas listed in §6.2.

## 8. Running

See [README.md](../README.md) for run commands covering default, PostgreSQL, S3, and all-backend configurations. Infrastructure is managed via `compose.storage-tests.yml`.
