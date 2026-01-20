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

Detailed test cases are defined in **`bpa/src/unit_test_plan.md`**.

The unit testing strategy focuses on isolating complex logic from the async runtime. Key areas include:

* **Status Reports:** RFC 9171 compliance.
* **Routing:** Table lookups and longest-prefix matching.
* **Policy:** QoS classification and queue management.
* **Storage:** Quota enforcement and eviction policies.
* **State Machines:** Channel backpressure and CLA lifecycle.

## 3. Pipeline Fuzzing (Existing)

*Scope: Robustness of the main processing loop.*
*Target: `bpa/fuzz/fuzz_targets/bpa.rs`*
*Detailed Plan: `bpa/fuzz/fuzz_test_plan.md`*

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

| Test ID | Steps | Expected Result |
 | ----- | ----- | ----- |
| **INT-BPA-01** | 1. Configure BPA with route `ipn:2.1` -> `file-cla` (Dir: `./outbox`).<br>2. Run `tools/ping -d ipn:2.1 -m "Hello"`. | 1. `ping` exits successfully.<br>2. A new file appears in `./outbox`.<br>3. File content is a valid Bundle with payload "Hello". |

### Suite B: Round-Trip Echo

*Objective: Verify bi-directional flow (App -> BPA -> CLA -> BPA -> App).*

| Test ID | Steps | Expected Result |
 | ----- | ----- | ----- |
| **INT-BPA-02** | 1. Configure BPA with loopback route.<br>2. Run `tools/ping` in "Echo Mode". | 1. Ping sends bundle.<br>2. BPA routes to CLA.<br>3. CLA "receives" (loopback) to BPA.<br>4. BPA delivers to Ping.<br>5. Ping reports `RTT = X ms`. |

### Suite C: Reassembly Logic

*Objective: Verify the BPA reassembles incoming fragments into a full bundle.*

| Test ID | Steps | Expected Result |
 | ----- | ----- | ----- |
| **INT-BPA-03** | 1. Manually generate 2 fragments for a "Hello" bundle.<br>2. Place fragments into `file-cla` inbox.<br>3. Run `tools/ping` in receive mode. | 1. BPA accepts fragments.<br>2. BPA reassembles payload.<br>3. `ping` receives single "Hello" bundle. |

## 5. Performance Benchmarks (REQ-13)

*Objective: Verify throughput requirements (>1000 bundles/sec).*

| Benchmark ID | Scenario | Target |
| ----- | ----- | ----- |
| **PERF-01** | **Memory Throughput** | Route 100k bundles (Memory Storage, Drop Route). | > 50k bundles/sec |
| **PERF-02** | **Storage I/O** | Route 10k bundles (Disk Storage, Loopback). | > 2k bundles/sec |
| **PERF-03** | **Reassembly Overhead** | Reassemble 1000 fragmented bundles (10 frags each). | > 1k bundles/sec |

## 6. Execution Matrix

| Test Level | Tooling | Coverage Focus |
 | ----- | ----- | ----- |
| **Unit** | `cargo test` | Status Reports, Route Lookup |
| **Fuzz** | `cargo fuzz` | Pipeline Stability, Deadlocks |
| **Benchmark** | `cargo bench` | Throughput (REQ-13) |
| **Integration** | `bash` + `tools/ping` | Full Stack Data Flow |
