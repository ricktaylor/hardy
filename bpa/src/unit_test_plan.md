# Unit Test Plan: BPA Logic

| Document Info | Details |
| ----- | ----- |
| **Functional Area** | BPA Internal Logic & Algorithms |
| **Module** | `hardy-bpa` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-1, REQ-2, REQ-6, REQ-7), `DTN-LLR_v1.1` (See Section 2) |
| **Parent Plan** | `bpa/src/test_plan.md` |
| **Test Suite ID** | UTP-BPA-01 |

## 1. Introduction

This document details the specific unit test cases for the `hardy-bpa` module. These tests target deterministic "Logic Islands" that can be verified without the full Tokio runtime or complex integration harnesses.

## 2. Requirements Mapping

The following requirements from **DTN-LLR_v1.1** are verified by this plan:

| LLR ID | Description |
| ----- | ----- |
| **1.1.30** | Processing must enforce bundle rewriting rules when discarding unrecognised blocks. |
| **1.1.33** | Processing must use Bundle Age block for expiry if Creation Time is zero. |
| **2.1.2** | Correctly remove BPSec target info when targeted block is removed. |
| **2.1.3** | Validate that Fragmented bundles do NOT contain BPSec extension blocks. |
| **6.1.1** | Correctly parse textual representation of `ipn` and `dtn` EID patterns. |
| **6.1.9** | Provide mechanism to prioritise routing rules. |
| **6.1.10** | Implement Equal Cost Multi-Path (ECMP). |
| **7.1.3** | Configurable discard policy (FIFO/Priority) when storage full. |

## 3. Unit Test Cases

### 3.1 Status Report Generation (RFC 9171 Sec 6.1)

*Objective: Verify that administrative records are generated correctly when bundles fail.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Route Missing** | Generate report for "No Route to Destination". | `src/dispatcher.rs` | Mock Bundle + `Reason::NoRoute` | Bundle containing correct Status Report payload & Reason Code. |
| **TTL Expired** | Generate report for "Lifetime Expired". | `src/dispatcher.rs` | Mock Bundle + `Reason::LifetimeExpired` | Bundle targeting original sender; "Lifetime Expired" flag set. |

### 3.2 Routing Table Logic (REQ-6)

*Objective: Verify the route lookup algorithms (Static & default routes).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Exact Match** | Lookup exact EID match. | `src/rib/find.rs` | Table: `ipn:1.1->CLA1`<br>Input: `ipn:1.1` | Result: `CLA1` |
| **Longest Prefix** | Lookup with overlapping routes. | `src/rib/find.rs` | Table: `ipn:2.*->CLA1`, `ipn:2.1->CLA2`<br>Input: `ipn:2.1` | Result: `CLA2` (Specific wins) |
| **Default Route** | Lookup with no match but default set. | `src/rib/find.rs` | Table: `default->CLA3`<br>Input: `ipn:99.99` | Result: `CLA3` |
| **ECMP Hashing** | Verify deterministic peer selection (REQ-6.1.10). | `src/rib/find.rs` | Route: `ipn:3.1 -> {CLA1, CLA2}`<br>Input: `Bundle(Flow=1)` vs `Bundle(Flow=2)` | Result: Deterministic selection (Same flow->Same CLA). |
| **Recursion Loop** | Verify detection of routing loops. | `src/rib/find.rs` | Route: `ipn:4.1 -> Via ipn:4.2`<br>`ipn:4.2 -> Via ipn:4.1` | Result: `Drop(NoKnownRoute)` |
| **Reflection** | Verify routing to previous node (REQ-6.1.8). | `src/rib/find.rs` | Route: `ipn:5.1 -> Reflect`<br>Input: `Bundle(Prev=ipn:9.9)` | Result: Route lookup for `ipn:9.9`. |
| **Local Ephemeral** | Verify drop for known-local but unregistered service. | `src/rib/local.rs` | Config: `NodeId=ipn:1.0`<br>Input: `ipn:1.99` (Unregistered) | Result: `Drop(DestUnavailable)` |
| **Action Precedence** | Verify Drop takes precedence over Via. | `src/rib/route.rs` | Route: `ipn:6.1 -> {Drop, Via X}` | Result: `Drop`. |
| **Local Action Sort** | Verify `Ord` impl for `local::Action`. | `src/rib/local.rs` | Input: `[Forward, Admin, Local]` | Result: `[Admin, Local, Forward]` |
| **Route Entry Sort** | Verify `Ord` impl for `route::Entry`. | `src/rib/route.rs` | Input: `[Via, Drop, Reflect]` | Result: `[Drop, Reflect, Via]` |
| **Implicit Routes** | Verify default routes created on startup. | `src/rib/local.rs` | Config: `NodeId=ipn:1.1` | Result: `Rib` contains `ipn:1.1 -> AdminEndpoint`. |
| **Impacted Subsets** | Verify `Rib::add` detects affected sub-routes. | `src/rib/mod.rs` | 1. Add `ipn:1.1 -> Via A`<br>2. Add `ipn:1.* -> Via B` | Result: `ipn:1.1` identified as impacted/reset. |

*(Note: Expiry/Time math is explicitly excluded here as it is verified in `hardy-bpv7`)*

### 3.3 Egress Policy Logic (QoS)

*Objective: Verify the `EgressPolicy` trait implementations and configuration parsing (REQ-6, LLR 6.1.9).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Flow Classification** | Map Flow Label to Queue Index. | `src/policy/mod.rs` | Policy: `Map { 100 -> Queue 1 }`<br>Input: `FlowLabel(100)` | Result: `Some(1)` |
| **Queue Bounds** | Handle invalid queue indices. | `src/policy/mod.rs` | Policy: `queue_count() = 2`<br>Input: `FlowLabel(999)` (maps to 5) | Result: `None` (Drop or Default) |

### 3.4 Service Registry Logic

*Objective: Verify internal state management for local application registrations.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Duplicate Reg** | Attempt to register an active ID. | `src/service_registry.rs` | 1. Reg `ipn:1.1` (Success)<br>2. Reg `ipn:1.1` | Result: `Error(AlreadyRegistered)` |
| **Cleanup** | Verify ID is freed on disconnect. | `src/service_registry.rs` | 1. Reg `ipn:1.1`<br>2. Drop Handle<br>3. Reg `ipn:1.1` | Result: `Success` |

### 3.5 Dispatcher Logic (Reassembly)

*Objective: Verify the fragment reassembly state machine in `dispatcher.rs`.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Basic Reassembly** | Reassemble 2 fragments in order. | `src/dispatcher.rs` | 1. Frag 1/2 (Offset 0)<br>2. Frag 2/2 (End) | Result: `Some(FullBundle)` |
| **Out-of-Order** | Reassemble fragments arriving reversed. | `src/dispatcher.rs` | 1. Frag 2/2<br>2. Frag 1/2 | Result: `Some(FullBundle)` |
| **Duplicate Data** | Handle overlapping/duplicate fragments. | `src/dispatcher.rs` | 1. Frag 1/2<br>2. Frag 1/2 | Result: `None` (Pending, no error) |
| **Missing Head** | Detect missing offset 0 fragment. | `src/dispatcher.rs` | 1. Frag 2/2 (End) | Result: `None` (Pending) or Log Warning. |
| **Length Mismatch** | Detect fragments claiming different total lengths. | `src/dispatcher.rs` | 1. Frag A (Total=100)<br>2. Frag B (Total=200) | Result: `None` (Drop/Error). |

### 3.6 Storage Logic (Quotas & Eviction)

*Objective: Verify storage constraints and garbage collection strategies (REQ-7, REQ-13).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Quota Enforcement** | Attempt to store bundle exceeding total capacity. | `src/storage/store.rs` | Storage: `Max=10MB`, `Used=9.9MB`<br>Input: `Store(200KB Bundle)` | Result: `Error(StorageFull)` or Eviction Triggered |
| **Eviction Policy (FIFO)** | Verify oldest bundle is dropped on full. | `src/storage/bundle_mem.rs` | Storage: `Full`<br>Input: `Store(NewBundle)` | Result: `Success` (Oldest deleted) |
| **Eviction Policy (Priority)** | Verify low priority is dropped for high priority. | `src/storage/bundle_mem.rs` | Storage: `Full` (All Normal prio)<br>Input: `Store(ExpeditedBundle)` | Result: `Success` (Normal bundle deleted) |
| **Double Delete** | Handle deletion of already removed bundle. | `src/storage/store.rs` | 1. `Delete(ID_A)`<br>2. `Delete(ID_A)` | Result: `Ok` (Idempotent) or `Error(NotFound)` |
| **Min Bundles Protection** | Verify `min_bundles` overrides byte quota. | `src/storage/bundle_mem.rs` | Config: `Max=1MB`, `Min=10`<br>Input: 5 bundles of 500KB. | Result: All stored (Over quota, but under count). |
| **Transaction Rollback** | Verify data cleanup on metadata failure. | `src/storage/store.rs` | 1. `SaveData` (Ok)<br>2. `InsertMeta` (Fail/Dup) | Result: `DeleteData` called automatically. |
| **Large Quota Config** | Verify `u64` parsing for >1TB limits. | `src/storage/config.rs` | Config: `Max=2TB` (2*10^12) | Result: Parsed correctly (no overflow). |

### 3.7 Channel State Machine (Backpressure)

*Objective: Verify the hybrid memory/storage queue logic in `channel.rs`.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Fast Path Saturation** | Fill memory channel to trigger Draining state. | `src/storage/channel.rs` | `Channel(Cap=10)`<br>Input: Send 11 bundles rapidly. | 1. 10 in Channel.<br>2. 11th triggers `Draining`.<br>3. Poller wakes. |
| **Congestion Signal** | Send while Draining to trigger Congested state. | `src/storage/channel.rs` | State: `Draining`<br>Input: `Send(Bundle)` | State becomes `Congested`. Poller loops again. |
| **Hysteresis Recovery** | Verify fast path re-opens only after drain. | `src/storage/channel.rs` | State: `Draining`, Channel: `Empty`<br>Action: Poller finishes. | State becomes `Open` (only when `< Cap/2`). |
| **Lazy Expiry** | Verify expired bundles are dropped during poll. | `src/storage/channel.rs` | Storage: Expired Bundle<br>Action: `poll_once` | Bundle not sent to channel; dropped. |
| **Close Safety** | Verify sends fail when closing. | `src/storage/channel.rs` | Action: `close()`<br>Input: `Send(Bundle)` | Result: `Err(SendError)`. |
| **Drop-to-Storage Integrity** | Verify bundle dropped from memory is retrieved from persistent storage. | `src/storage/channel.rs` | 1. Fill Channel.<br>2. `Send(Bundle X)` (triggers Draining).<br>3. Drain Channel. | Result: `Bundle X` arrives via Poller. |
| **Hybrid Duplication** | Verify bundles already in channel are not re-injected by poller. | `src/storage/channel.rs` | 1. `Send(A)` (Success).<br>2. `Send(B)` (Full -> Draining).<br>3. Poller runs. | Result: Receiver gets `A, B`. (Not `A, A, B`). |
| **Ordering Preservation** | Verify FIFO/Priority is maintained during mode switch. | `src/storage/channel.rs` | 1. `Send(A)` (Fast).<br>2. `Send(B)` (Slow/Drop). | Result: Receiver gets `A` then `B`. |
| **Status Consistency** | Verify bundles with mismatched status are filtered. | `src/storage/channel.rs` | 1. Storage has Bundle Y (Status=Deleted).<br>2. `poll_once` (Target=Queued). | Result: Bundle Y is dropped/ignored. |
| **Zombie Task Leak** | Verify poller task exits when Sender is dropped (or requires explicit close). | `src/storage/channel.rs` | 1. Create Channel.<br>2. Drop `Sender`. | Result: Task terminates (or warn if not). |

### 3.8 CLA Registry & Peer Logic

*Objective: Verify the generic CLA management and peer state machines in `cla/registry.rs` and `cla/peers.rs`.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Address Parsing** | Verify `ClaAddress` conversion logic. | `src/cla/mod.rs` | Input: `(Tcp, "127.0.0.1:80")` | Result: `Ok(SocketAddr)`. |
| **Duplicate Registration** | Register CLA with existing name. | `src/cla/registry.rs` | 1. Reg "tcp" (Ok)<br>2. Reg "tcp" | Result: `Error(AlreadyExists)` |
| **Peer Lifecycle** | Verify RIB updates on peer add/remove. | `src/cla/registry.rs` | 1. `add_peer(NodeA)`<br>2. `remove_peer(NodeA)` | 1. RIB contains NodeA.<br>2. RIB does not contain NodeA. |
| **Cascading Cleanup** | Verify unregistering CLA removes peers. | `src/cla/registry.rs` | 1. Reg CLA -> Add Peer A.<br>2. Unregister CLA. | Result: Peer A removed from RIB. |
| **Queue Selection** | Verify Policy maps to correct CLA queue. | `src/cla/peers.rs` | Policy: `Classify -> 1`<br>CLA: 2 Queues | Result: Bundle sent via Queue 1. |
| **Queue Fallback** | Verify fallback to default queue on invalid index. | `src/cla/peers.rs` | Policy: `Classify -> 99`<br>CLA: 2 Queues | Result: Bundle sent via Queue 0 (Default). |

### 3.9 Reaper Logic (TTL Scheduling)

*Objective: Verify the bounded priority cache for bundle expiry in `reaper.rs` (REQ-1.1.33).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Cache Ordering** | Verify `CacheEntry` sorts by time. | `src/storage/reaper.rs` | Input: `[T+10, T+5, T+20]` | Result: `[T+5, T+10, T+20]` |
| **Cache Saturation** | Verify "Keep Soonest" eviction policy. | `src/storage/reaper.rs` | Cache: Full `[T+10...T+20]`<br>Input: `T+5` | Result: `T+20` evicted, `T+5` inserted. |
| **Cache Rejection** | Verify later expiry is ignored if full. | `src/storage/reaper.rs` | Cache: Full `[T+10...T+20]`<br>Input: `T+30` | Result: `T+30` ignored (Cache unchanged). |
| **Wakeup Trigger** | Verify wakeup signal on new soonest expiry. | `src/storage/reaper.rs` | Cache: `[T+10]`<br>Input: `T+5` | Result: `notify` triggered. |

### 3.10 Node ID Logic

*Objective: Verify Node ID configuration validation and admin endpoint resolution in `node_ids.rs`.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Single Scheme Enforce** | Verify error on multiple IPN IDs. | `src/node_ids.rs` | Input: `[ipn:1.1, ipn:2.2]` | Result: `Error(MultipleIpnNodeIds)` |
| **Invalid Types** | Verify rejection of Local/Null nodes. | `src/node_ids.rs` | Input: `[dtn:none]` | Result: `Error(NullEndpoint)` |
| **Admin Resolution (IPN)** | Resolve admin EID for IPN destination. | `src/node_ids.rs` | Config: `ipn:1.1`<br>Dest: `ipn:2.1` | Result: `ipn:1.0` |
| **Admin Resolution (DTN)** | Resolve admin EID for DTN destination. | `src/node_ids.rs` | Config: `dtn:node`<br>Dest: `dtn:other/svc` | Result: `dtn:node` |

### 3.11 Bundle Time Math

*Objective: Verify wrapper logic for bundle age and expiry in `bundle.rs`.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Age Fallback** | Verify creation time derived from Age. | `src/bundle.rs` | TS: `0`, Age: `10s`, Recv: `T=100` | Result: Creation = `T=90`. |
| **Expiry Calculation** | Verify expiry time summation. | `src/bundle.rs` | Creation: `T=100`, Life: `50s` | Result: Expiry = `T=150`. |

### 3.12 BPSec Policy Logic (REQ-2)

*Objective: Verify BPA enforcement of security constraints (LLR 2.1.2, 2.1.3).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Fragment Security** | Reject fragmentation if BPSec blocks present. | `src/fragmentation.rs` | Bundle with BIB/BCB + Request Fragment | `Error(CannotFragmentSecureBundle)` |
| **Target Cleanup** | Remove security info when target block is dropped. | `src/security.rs` | Bundle with BIB targeting Payload + Drop Payload | Bundle with BIB removed. |

### 3.13 Bundle Manipulation (Canonicalization)

*Objective: Verify bundle rewriting rules (LLR 1.1.30).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Unknown Block Drop** | Remove unknown block marked "Discard if Unknown". | `src/process.rs` | Bundle with Unknown Block (Flag=Discard) | Bundle without Block. |
| **Unknown Block Keep** | Keep unknown block NOT marked "Discard". | `src/process.rs` | Bundle with Unknown Block (Flag=Keep) | Bundle with Block preserved. |

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-bpa`

* **Pass Criteria:** All tests listed above must return `ok`.

* **Coverage Target:** > 85% line coverage for `src/rib/`, `src/storage/`, `src/policy/`.
