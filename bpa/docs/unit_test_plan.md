# Unit Test Plan: BPA Logic

| Document Info | Details |
| ----- | ----- |
| **Functional Area** | BPA Internal Logic & Algorithms |
| **Module** | `hardy-bpa` |
| **Requirements Ref** | [REQ-1](../../docs/requirements.md#req-1-full-compliance-with-rfc9171), [REQ-2](../../docs/requirements.md#req-2-full-compliance-with-rfc9172-and-rfc9173), [REQ-6](../../docs/requirements.md#req-6-time-variant-routing-api), [REQ-7](../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [LLR](../../docs/requirements.md#part-3-low-level-requirements-llr) |
| **Parent Plan** | `bpa/docs/component_test_plan.md` |
| **Test Suite ID** | UTP-BPA-01 |
| **Revision** | Rev 3 (2026-04-13) — Source file paths updated; §3.12/§3.13 delegated to bpv7 suite |
| **Version** | 1.0 |

## 1. Introduction

This document details the specific unit test cases for the `hardy-bpa` module. These tests target deterministic "Logic Islands" that can be verified without the full Tokio runtime or complex integration harnesses.

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by this plan:

| LLR ID | Description |
| ----- | ----- |
| [**1.1.30**](../../docs/requirements.md#bpv7-bundle-processing-11) | Processing must enforce bundle rewriting rules when discarding unrecognised blocks. |
| [**1.1.33**](../../docs/requirements.md#bpv7-bundle-processing-11) | Processing must use Bundle Age block for expiry if Creation Time is zero. |
| [**2.1.2**](../../docs/requirements.md#bpsec-21---optional-for-initial-development) | Correctly remove BPSec target info when targeted block is removed. |
| [**2.1.3**](../../docs/requirements.md#bpsec-21---optional-for-initial-development) | Validate that Fragmented bundles do NOT contain BPSec extension blocks. |
| [**6.1.1**](../../docs/requirements.md#eid-patterns-61) | Correctly parse textual representation of `ipn` and `dtn` EID patterns. |
| [**6.1.9**](../../docs/requirements.md#routing-61) | Provide mechanism to prioritise routing rules. |
| [**6.1.10**](../../docs/requirements.md#routing-61) | Implement Equal Cost Multi-Path (ECMP). |
| [**7.1.3**](../../docs/requirements.md#local-disk-storage-71) | Configurable discard policy (FIFO/Priority) when storage full. |

## 3. Unit Test Cases

### 3.1 Status Report Generation (RFC 9171 Sec 6.1)

*Objective: Verify that administrative records are generated correctly when bundles fail.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Route Missing** | Generate report for "No Route to Destination". | `src/dispatcher/report.rs` | Mock Bundle + `Reason::NoRoute` | Bundle containing correct Status Report payload & Reason Code. |
| **TTL Expired** | Generate report for "Lifetime Expired". | `src/dispatcher/report.rs` | Mock Bundle + `Reason::LifetimeExpired` | Bundle targeting original sender; "Lifetime Expired" flag set. |

### 3.2 Routing Table Logic (REQ-6)

*Objective: Verify the route lookup algorithms (Static & default routes).*

All 15 tests implemented. ECMP uses per-instance `RandomState` for deterministic peer selection.

| Test Scenario | Description | Source File | Status |
| ----- | ----- | ----- | ----- |
| **Exact Match** | Lookup exact EID match via local forward. | `src/rib/find.rs` | Done |
| **Default Route** | Catch-all `*:**` Via route resolves unknown destinations. | `src/rib/find.rs` | Done |
| **No Route** | No routes installed — returns None (wait for route). | `src/rib/find.rs` | Done |
| **ECMP Hashing** | Deterministic peer selection across lookups (REQ-6.1.10). | `src/rib/find.rs` | Done |
| **Recursion Loop** | Circular Via routes detected → Drop(NoKnownRoute). | `src/rib/find.rs` | Done |
| **Reflection** | Reflect route sends bundle back via previous node's peer (REQ-6.1.8). | `src/rib/find.rs` | Done |
| **No Double Reflect** | Both destination and previous-hop reflect → None. | `src/rib/find.rs` | Done |
| **Local Ephemeral** | Known-local EID with no service → Drop(DestUnavailable). | `src/rib/local.rs` | Done |
| **Action Precedence** | Verify Drop < Reflect < Via ordering. | `src/rib/route.rs` | Done |
| **Local Action Sort** | Verify `Ord` impl for `local::Action`. | `src/rib/local.rs` | Done |
| **Route Entry Sort** | Verify `Ord` impl for `route::Entry` in BTreeSet. | `src/rib/route.rs` | Done |
| **Entry Source Tiebreak** | Same action, different source — sorted alphabetically. | `src/rib/route.rs` | Done |
| **Entry Dedup** | Duplicate entries rejected by BTreeSet. | `src/rib/route.rs` | Done |
| **Implicit Routes** | Verify default routes created on startup. | `src/rib/local.rs` | Done |
| **Impacted Subsets** | Verify `Rib::add` inserts routes at correct priority. | `src/rib/mod.rs` | Done |

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
| **Duplicate Reg** | Attempt to register an active ID. | `src/services/registry.rs` | 1. Reg `ipn:1.1` (Success)<br>2. Reg `ipn:1.1` | Result: `Error(AlreadyRegistered)` |
| **Cleanup** | Verify ID is freed on disconnect. | `src/services/registry.rs` | 1. Reg `ipn:1.1`<br>2. Drop Handle<br>3. Reg `ipn:1.1` | Result: `Success` |

### 3.5 Dispatcher Logic (Reassembly)

*Objective: Verify the fragment reassembly state machine in `dispatcher.rs`.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Basic Reassembly** | Reassemble 2 fragments in order. | `src/storage/adu_reassembly.rs` | 1. Frag 1/2 (Offset 0)<br>2. Frag 2/2 (End) | Result: `Some(FullBundle)` |
| **Out-of-Order** | Reassemble fragments arriving reversed. | `src/storage/adu_reassembly.rs` | 1. Frag 2/2<br>2. Frag 1/2 | Result: `Some(FullBundle)` |
| **Duplicate Data** | Handle overlapping/duplicate fragments. | `src/storage/adu_reassembly.rs` | 1. Frag 1/2<br>2. Frag 1/2 | Result: `None` (Pending, no error) |
| **Missing Head** | Detect missing offset 0 fragment. | `src/storage/adu_reassembly.rs` | 1. Frag 2/2 (End) | Result: `None` (Pending) or Log Warning. |
| **Length Mismatch** | Detect fragments claiming different total lengths. | `src/storage/adu_reassembly.rs` | 1. Frag A (Total=100)<br>2. Frag B (Total=200) | Result: `None` (Drop/Error). |

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
| **Large Quota Config** | Verify `u64` parsing for >1TB limits. | `src/storage/bundle_mem.rs` | Config: `Max=2TB` (2*10^12) | Result: Parsed correctly (no overflow). |

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
| **Age Fallback** | Verify creation time derived from Age. | `src/bundle/core.rs` | TS: `0`, Age: `10s`, Recv: `T=100` | Result: Creation = `T=90`. |
| **Expiry Calculation** | Verify expiry time summation. | `src/bundle/core.rs` | Creation: `T=100`, Life: `50s` | Result: Expiry = `T=150`. |

### 3.12 BPSec Policy Logic (REQ-2)

*Objective: Verify BPA enforcement of security constraints (LLR 2.1.2, 2.1.3).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Fragment Security** | Reject fragmentation if BPSec blocks present. | `bpv7/src/bpsec/signer.rs` | Bundle with BIB/BCB + Request Fragment | `Error(CannotFragmentSecureBundle)` |
| **Target Cleanup** | Remove security info when target block is dropped. | `src/dispatcher/dispatch.rs` | Bundle with BIB targeting Payload + Drop Payload | Bundle with BIB removed. |

**Note (Rev 3):** Fragment Security (LLR 2.1.3) is a sender constraint enforced by `bpv7/src/bpsec/signer.rs:75` — not a BPA-level validation. Target Cleanup (LLR 2.1.2) is verified by the bpv7 unit tests (`test_bib_removal_and_readd`, `test_bcb_without_bib_removal`). Both scenarios are covered by the bpv7 test suite ([`UTP-BPSEC-01`](../../bpv7/docs/unit_test_plan_bpsec.md)); no separate BPA tests are required.

### 3.13 Bundle Manipulation (Canonicalization)

*Objective: Verify bundle rewriting rules (LLR 1.1.30).*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Unknown Block Drop** | Remove unknown block marked "Discard if Unknown". | `src/dispatcher/dispatch.rs` | Bundle with Unknown Block (Flag=Discard) | Bundle without Block. |
| **Unknown Block Keep** | Keep unknown block NOT marked "Discard". | `src/dispatcher/dispatch.rs` | Bundle with Unknown Block (Flag=Keep) | Bundle with Block preserved. |

**Note (Rev 3):** Unknown block handling (LLR 1.1.30) is implemented by the bpv7 parser and verified by `bpv7/src/bundle/parse.rs::unknown_block_discard` and the CLI integration test REWRITE-01 ([`COMP-BPV7-CLI-01`](../../bpv7/docs/component_test_plan.md)). The BPA delegates to the parser — no separate BPA tests are required.

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-bpa`

* **Pass Criteria:** All tests listed above must return `ok`.

* **Coverage Target:** > 85% line coverage for `src/rib/`, `src/storage/`, `src/policy/`.
