# Component Test Plan: Bundle Protocol Agent (BPA)

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Bundle Routing & Processing Pipeline |
| **Module** | `hardy-bpa` |
| **Requirements Ref** | [REQ-1](../../docs/requirements.md#req-1-full-compliance-with-rfc9171), [REQ-6](../../docs/requirements.md#req-6-time-variant-routing-api), [REQ-13](../../docs/requirements.md#req-13-performance), [REQ-14](../../docs/requirements.md#req-14-reliability), [LLR 6.x](../../docs/requirements.md#eid-patterns-61) |
| **Standard Ref** | RFC 9171 (BPv7 Processing) |
| **Test Suite ID** | PLAN-BPA-01 |
| **Version** | 1.2 |

## 1. Introduction

This document details the testing strategy for the `hardy-bpa` module. This module is the asynchronous "brain" of the router, responsible for storage, routing, and lifecycle management.

**Strategy Shift:**
Unlike the parser modules (`bpv7`, `cbor`), the BPA is tested primarily through **Integration** and **Fuzzing** to verify pipeline behavior. Unit tests are reserved strictly for isolated algorithmic logic.

## 2. Unit Testing Strategy

*Scope: Deterministic algorithms that do not require the Tokio runtime.*

Detailed test cases are defined in **[`UTP-BPA-01`](unit_test_plan.md)**.

The unit testing strategy focuses on isolating complex logic from the async runtime. Key areas include:

* **Status Reports:** RFC 9171 compliance.
* **Routing:** Table lookups and longest-prefix matching.
* **Policy:** QoS classification and queue management.
* **Storage:** Quota enforcement and eviction policies.
* **State Machines:** Channel backpressure and CLA lifecycle.

## 3. Pipeline Fuzzing (Existing)

*Scope: Robustness of the main processing loop.*
*Target: `bpa/fuzz/fuzz_targets/bpa.rs`*
*Detailed Plan: [`FUZZ-BPA-01`](../docs/fuzz_test_plan.md)*

| Target Name | Description | Vulnerability Class | Pass Criteria |
 | ----- | ----- | ----- | ----- |
| **`bpa`** | Feeds the BPA random events (Bundle Received, Timer Fired, Route Updates). | **Async Deadlocks:** `select!` starvation.<br>**State Corruption:** Invalid transitions (e.g., deleting a locked bundle). | 24 Hours execution with 0 Panics or Timeouts. |

## 4. Integration Test Suites (Grey-Box Tools)

*Scope: Verification of the full stack using `hardy-ping` and `file-cla`.*

**Test Setup:**

* **BPA Config:** Single node, configured with `file-cla` storage/routing.
* **Tooling:** `tools/ping` (Generator), `file-cla` (FileSystem Convergence Layer).

### Suite A: The "File-Loopback" Test

*Objective: Verify the BPA can accept a bundle from an application and route it to a CLA.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **INT-BPA-01** | **App-to-CLA Routing** | 1. Configure BPA with route `ipn:2.1` -> `file-cla` (Dir: `./outbox`).<br>2. Run `tools/ping -d ipn:2.1 -m "Hello"`.<br>3. Check `./outbox` directory. | 1. `ping` exits successfully.<br>2. A new file appears in `./outbox`.<br>3. File content is a valid Bundle with payload "Hello". |

### Suite B: Round-Trip Echo

*Objective: Verify bi-directional flow (App -> BPA -> CLA -> BPA -> App).*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **INT-BPA-02** | **Echo Round-Trip** | 1. Configure BPA with loopback route.<br>2. Run `tools/ping` in "Echo Mode".<br>3. Observe ping output. | 1. Ping sends bundle.<br>2. BPA routes to CLA.<br>3. CLA "receives" (loopback) to BPA.<br>4. BPA delivers to Ping.<br>5. Ping reports `RTT = X ms`. |

### Suite C: Reassembly Logic

*Objective: Verify the BPA reassembles incoming fragments into a full bundle.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **INT-BPA-03** | **Fragment Reassembly** | 1. Manually generate 2 fragments for a "Hello" bundle.<br>2. Place fragments into `file-cla` inbox.<br>3. Run `tools/ping` in receive mode. | 1. BPA accepts fragments.<br>2. BPA reassembles payload.<br>3. `ping` receives single "Hello" bundle. |

### Suite D: Filter Dispositions

*Objective: the filter module's failure and Drop contract holds through a running BPA (`bpa/tests/filter_dispositions.rs`, plus the extent-consistency test in `bpa/tests/pipeline.rs`).*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **INT-BPA-04** | **Deliver Drop contract** | 1. Register a Deliver Verifier dropping one destination, with and without a reason.<br>2. Deliver a report-requesting bundle to it via a CLA. | `Drop(Some(r))`: exactly one deletion report carrying `r`; `Drop(None)`: no report originated. Either way the bundle is not delivered, and its record is tombstoned. (Nothing at Egress drops a bundle.) |
| **INT-BPA-06** | **Egress filter bundle/data consistency** | 1. Register an Egress Rewriter checking block extents against the bytes and inserting a block (`bpa/tests/pipeline.rs`).<br>2. Forward a locally-originated bundle, so the per-hop writes then insert a Previous Node block over the Rewriter's output. | The Rewriter sees the stored (bundle, data) pair consistently, and the transmitted bundle carries the Rewriter's insert, this node's Previous Node and the payload, each decoding from its own extent. |
| **INT-BPA-13** | **Undecodable stored bytes** | — (no pipeline test). | Fatal at Egress and Deliver: the bytes were validated at ingress, so a decode failure in an output chain or the per-hop writes is a BPA bug or storage corruption. Pinned at function level ([unit plan](unit_test_plan.md) §3.14): a panic inside a running BPA aborts the test process. |
| **INT-BPA-14** | **Editor refusals on attacker-chosen primaries** | 1. Register a Rewriter inserting a `report_on_failure` block.<br>2. Deliver an admin-record transit bundle (Egress) and a null-source local bundle (Deliver). | The insert is refused with `InvalidFlags` and the bundle passes unedited; the node does not abort. |
| **INT-BPA-15** | **Per-hop writes supersede an Egress Rewriter** | 1. Register an Egress Rewriter removing the Hop Count and Bundle Age blocks.<br>2. Forward a bundle with no creation clock, carrying both, via a CLA. | The Rewriter's removals are accepted, and the transmitted bundle carries both blocks again as this hop writes them: the Hop Count incremented, the Bundle Age at least its received value. |

### Suite E: Service Delivery Failure

*Objective: a delivery the service fails leaves the bundle in custody (`bpa/tests/pipeline.rs`).*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **INT-BPA-05** | **`on_deliver` returns `Err`** | 1. Register an application whose `on_deliver` fails.<br>2. Originate a bundle to it from a second application.<br>3. Unregister the failing application and register a working one on the same service id. | The failed delivery parks the bundle `WaitingForService` rather than reporting it delivered and deleting it; the working receiver gets it re-delivered, payload intact. |

### Suite F: Deferred CLA Transfer Outcomes

*Objective: a transfer the CLA answers `Accepted` stays in the BPA's custody until its outcome arrives, and resolves exactly once (see [Deferred CLA Transfer Outcomes](design.md#deferred-cla-transfer-outcomes); `bpa/tests/pipeline.rs`, `bpa/tests/forward_expiry.rs`).*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **INT-BPA-07** | **Outcome `Failed`** | 1. Register a CLA that answers its first offer `Accepted`.<br>2. Ingress a bundle routed to its peer.<br>3. Report the transfer `Failed` through `Sink::transfer_outcome`. | The bundle re-enters dispatch and is re-offered to the CLA; it is never dropped. |
| **INT-BPA-08** | **Outcome `Completed`** | 1–2. As INT-BPA-07.<br>3. Report the transfer `Completed`, then report it `Failed`.<br>4. Ingress the same bundle again. | No re-offer: the late second outcome is ignored, and the tombstone drops the re-arrival as a duplicate. |
| **INT-BPA-09** | **Peer removed mid-transfer** | 1–2. As INT-BPA-07.<br>3. Remove the peer, then add it back. | Removal resolves the transfer as outcome-unknown: the bundle returns to `Waiting` and is re-offered once the peer is back. |
| **INT-BPA-10** | **Outcome from a CLA that does not own the transfer** | 1–2. As INT-BPA-07, with a second CLA holding its own peer.<br>3. Report an outcome for a bundle id the BPA has never seen, then the transfer's outcome from the second CLA, then `Failed` from the owning CLA. | The unknown-id and non-owner outcomes are ignored without error; the owner's outcome is honoured and the bundle is re-offered to the owner alone. |
| **INT-BPA-11** | **Expiry mid-transfer** | 1. Forward a short-lifetime, deletion-report-requesting bundle to a CLA that answers `Accepted` and holds the transfer past expiry.<br>2. Report the outcome `Completed`, or `Failed`.<br>3. With a reaper cache of two, hold two never-resolved transfers ahead of a reapable bundle. | The reaper defers the bundle while the CLA owns the transfer, and the outcome resolves it exactly once: `Completed` reports the hand-off (`NoAdditionalInformation`), never `LifetimeExpired`; `Failed` re-enters dispatch, whose expiry checkpoint drops it as `LifetimeExpired`. Deferred transfers never starve the reaper: the reapable bundle is still reaped and reported. |

### Suite G: Streamed Doors

*Objective: the streamed service and CLA doors behave as their whole-buffer forms, and a stream that ends early is cancelled or refused (`bpa/tests/pipeline.rs`).*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **INT-BPA-12** | **Streamed origination and ingress** | 1. Originate through `ServiceSink::send` in several segments; drop the producer before `Final`; unregister the service while a send is parked; send a bundle whose source is not the service's endpoint.<br>2. Dispatch through `cla::Sink::dispatch` in several segments; unregister the CLA while a stream is parked; drop the producer before `Final`, including after a whole bundle in `Next` and after every byte of an oversized one. | Multi-segment origination and ingress match their whole-buffer forms (origination returns the built bundle's id and forwards it; ingress delivers locally). A producer dropped before `Final` cancels an origination (`StreamCancelled`, nothing enters custody) and is refused at ingress (`Acceptance::Refused`, nothing delivered), even once every byte of the bundle has arrived. Unregistration wakes a parked consumer with `StreamCancelled` at once. The spoofed source is rejected. |

## 5. Performance Benchmarks (REQ-13)

*Objective: Verify throughput requirements (>1000 bundles/sec).*

| Benchmark ID | Scenario | Target |
| ----- | ----- | ----- |
| **PERF-01** | **Memory Throughput** | Route 100k bundles (Memory Storage, Drop Route). | > 50k bundles/sec |
| **PERF-02** | **Storage I/O** | Route 10k bundles (Disk Storage, Loopback). | > 2k bundles/sec |
| **PERF-03** | **Reassembly Overhead** | Reassemble 1000 fragmented bundles (10 frags each). | > 1k bundles/sec |

### 5.1 Latency Profiling

*Objective: Measure latency distribution under various conditions.*

| Benchmark ID | Scenario | Procedure | Target |
| :--- | :--- | :--- | :--- |
| **PERF-LAT-01** | **Single-Hop Latency (Baseline)** | Send 10,000 small bundles (1KB) via loopback. Measure per-bundle RTT using `hardy-ping`. | P50 < 1ms, P95 < 5ms, P99 < 10ms |
| **PERF-LAT-02** | **Latency Under Load** | Establish baseline latency. Increase load to 80% throughput capacity. Measure latency degradation. | P99 latency < 10x baseline |

### 5.2 BPSec Performance

*Objective: Quantify cryptographic overhead for security operations.*

| Benchmark ID | Scenario | Procedure | Target |
| :--- | :--- | :--- | :--- |
| **PERF-SEC-01** | **BIB Signing Overhead** | Measure throughput without BIB, then with HMAC-SHA256, then HMAC-SHA512. | Throughput > 10k bundles/sec (1KB bundles) |
| **PERF-SEC-02** | **BCB Encryption Overhead** | Measure throughput without BCB, then with AES-GCM-128, then AES-GCM-256. | Large bundle encryption > 1GB/sec |
| **PERF-SEC-03** | **Combined BIB + BCB** | Apply both integrity and confidentiality. Measure combined overhead. | Overhead < 2x individual operations |

## 6. Execution Matrix

| Test Level | Tooling | Coverage Focus |
 | ----- | ----- | ----- |
| **Unit** | `cargo test` | Status Reports, Route Lookup |
| **Fuzz** | `cargo fuzz` | Pipeline Stability, Deadlocks |
| **Benchmark** | `cargo bench` | Throughput (REQ-13), measured; nothing gates on it |
| **Integration** | `bash` + `tools/ping` | Full Stack Data Flow |
