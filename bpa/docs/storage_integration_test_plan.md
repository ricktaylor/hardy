# Test Plan: Storage Integration (Metadata & Bundles)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Persistence Layer (Metadata & Payloads) |
| **Module** | `hardy-bpa` |
| **Interfaces** | `crate::storage::MetadataStorage`, `crate::storage::BundleStorage` |
| **Requirements Ref** | [REQ-7](../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [REQ-8](../../docs/requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage), [REQ-9](../../docs/requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage), [LLR 7.x](../../docs/requirements.md#314-local-disk-storage-parent-req-7), [LLR 9.x](../../docs/requirements.md#316-s3-storage-parent-req-9) |
| **Test Suite ID** | PLAN-STORE-01 |

## 1. Introduction

This document details the integration testing strategy for the persistence layer of the BPA. It covers two distinct traits:

1. **`MetadataStorage`**: Stores structured bundle state (status, routing flags, timestamps).
2. **`BundleStorage`**: Stores opaque binary bundle payloads (BLOBs).

The tests defined here are intended to be run against **all** implementations of these traits via a common harness.

## 2. Requirements Mapping

| ID | Requirement | Test Coverage |
| :--- | :--- | :--- |
| [**REQ-7**](../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage) | Support for local filesystem (SQLite/Local Disk). | Verified by running suite against `sqlite-storage`, `localdisk-storage`. |
| [**REQ-8**](../../docs/requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage) | Support for PostgreSQL. | Verified by running suite against `postgres-storage`. |
| [**REQ-9**](../../docs/requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage) | Support for S3 (Bundle Storage). | Verified by running suite against `s3-storage`. |
| [**7.2.1**](../../docs/requirements.md#315-sqlite-storage-parent-req-7) | Store/Retrieve metadata. | Covered by **Suite A (Metadata CRUD)**. |
| [**7.1.1**](../../docs/requirements.md#314-local-disk-storage-parent-req-7) | Store/Retrieve payloads. | Covered by **Suite D (Bundle CRUD)**. |
| [**7.1.3**](../../docs/requirements.md#314-local-disk-storage-parent-req-7) | Configurable discard policy. | Covered by **Suite B (Polling)**. |

## 3. Metadata Storage Suites

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

### Suite C: State Transitions & Bulk Ops

*Objective: Verify complex state management operations required by the BPA.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **META-05** | **Confirm Exists (Recovery)** | _Persistent backends only._<br>1. Insert bundle A.<br>2. Call `start_recovery()` (marks A unconfirmed).<br>3. Call `confirm_exists(A)`.<br>4. Call `confirm_exists(B)` (never inserted).<br>5. Call `remove_unconfirmed(tx)`. | 1. Step 3 returns `Some(metadata)`.<br>2. Step 4 returns `None`.<br>3. Step 5 removes nothing (A was confirmed).<br>4. `get(A)` still returns `Some`. |
| **META-11** | **Reset Peer Queue** | 1. Insert Bundle A (Status=`ForwardPending { peer: 100, queue: Some(0) }`).<br>2. Insert Bundle B (Status=`ForwardPending { peer: 200, queue: Some(0) }`).<br>3. Call `reset_peer_queue(100)`. | 1. `reset_peer_queue` returns `true`.<br>2. Bundle A status becomes `Waiting`.<br>3. Bundle B status remains `ForwardPending`. |
| **META-12** | **Recovery** | 1. Call `start_recovery()`. | 1. Returns `()` (No panic/error). |
| **META-13** | **Remove Unconfirmed** | 1. Insert Bundle A.<br>2. Call `remove_unconfirmed(tx)`. | 1. Returns `Ok`.<br>2. `tx` receives bundles (if implementation supports unconfirmed state). |

## 4. Bundle Storage Suites

### Suite D: Payload Operations

*Objective: Verify the storage and retrieval of binary bundle data.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **BLOB-01** | **Save & Load** | 1. Generate random bytes (1KB).<br>2. Call `save(bytes)`.<br>3. Call `load(name)`. | 1. `save` returns a storage name (string).<br>2. `load` returns `Some(bytes)`.<br>3. Bytes match exactly. |
| **BLOB-02** | **Delete** | 1. Save bytes.<br>2. Call `delete(name)`.<br>3. Call `load(name)`. | 1. `delete` returns `Ok`.<br>2. `load` returns `None`. |
| **BLOB-03** | **Missing Load** | 1. Call `load("non-existent")`. | 1. Returns `Ok(None)` (Not an error). |
| **BLOB-04** | **Recovery Scan** | 1. Save Blob A.<br>2. Save Blob B.<br>3. Call `recover(tx)`. | 1. `tx` receives entries for A and B.<br>2. Entries contain correct storage name and timestamp (`RecoveryResponse = (Arc<str>, OffsetDateTime)`). |
