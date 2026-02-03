# Component Test Plan: Bundle Protocol Agent (BPA)

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Bundle Routing & Processing Pipeline |
| **Module** | `hardy-bpa` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-1, REQ-6, REQ-13, REQ-14), `DTN-LLR_v1.1` (Section 5, 6) |
| **Standard Ref** | RFC 9171 (BPv7 Processing) |
| **Test Suite ID** | PLAN-BPA-01 |

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
| **Benchmark** | `cargo bench` | Throughput (REQ-13) |
| **Integration** | `bash` + `tools/ping` | Full Stack Data Flow |
