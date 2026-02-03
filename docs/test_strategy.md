# Test Strategy: Cloud-based DTN Router (Hardy)

| Document Info | Details |
 | ----- | ----- |
| **Project** | Hardy (Cloud-based DTN Router) |
| **Repository** | `github.com/ricktaylor/hardy` |
| **Version** | 1.1 |
| **Status** | DRAFT |

## 1. Introduction

This document defines the strategic approach for verifying the `hardy` Delay Tolerant Networking (DTN) router. It outlines the testing methodology, environments, and tools required to ensure compliance with **RFC 9171 (BPv7)**, **RFC 9172/3 (BPSec)**, and **RFC 9174 (TCPCLv4)**.

This strategy employs a **Modular Verification Architecture**, where individual functional areas (CBOR, BPv7, Security, Addressing) have dedicated Test Plans for Unit and Fuzz testing, culminating in System-level verification.

It is designed to verify the High-Level Requirements (HLR) and Low-Level Requirements (LLR) defined in **[requirements.md](requirements.md)**.

## 2. Document Hierarchy (Test Plan Architecture)

This Strategy is the parent document. Verification is executed according to the following child plans:

| Functional Area | Test Type | Document ID | Scope |
| ----- | ----- | ----- | ----- |
| **CBOR** | Unit | [`UTP-CBOR-01`](../cbor/docs/unit_test_plan.md) | RFC 8949 compliance, canonicalization. |
| **CBOR** | Fuzz | [`FUZZ-CBOR-01`](../cbor/docs/fuzz_test_plan.md) | Decoder robustness (Stack/OOM). |
| **BPv7 Core** | Unit | [`UTP-BPV7-01`](../bpv7/docs/unit_test_plan.md) | RFC 9171 parsing, factories, EID logic. |
| **BPv7 Core** | Fuzz | [`FUZZ-BPV7-01`](../bpv7/docs/fuzz_test_plan.md) | Bundle parsing, EID string/CBOR parsing. |
| **BPv7 Core** | Component | [`COMP-BPV7-CLI-01`](../bpv7/docs/component_test_plan.md) | CLI-driven verification of library logic. |
| **BPSec** | Unit | [`UTP-BPSEC-01`](../bpv7/src/bpsec/unit_test_plan.md) | RFC 9172/3 Integrity & Confidentiality. |
| **EID Patterns** | Unit | [`UTP-PAT-01`](../eid-patterns/docs/unit_test_plan.md) | Draft-05 Pattern matching logic. |
| **EID Patterns** | Fuzz | [`FUZZ-PAT-01`](../eid-patterns/docs/fuzz_test_plan.md) | Pattern DSL parser robustness. |
| **BPA Logic** | Unit | [`UTP-BPA-01`](../bpa/src/unit_test_plan.md) | BPA internal algorithms (Routing, Policy). |
| **BPA Logic** | Integration | [`PLAN-BPA-01`](../bpa/src/component_test_plan.md) | Routing, Pipeline, Performance Benchmarks. |
| **BPA Pipeline** | Fuzz | [`FUZZ-BPA-01`](../bpa/docs/fuzz_test_plan.md) | Async pipeline stability and deadlocks. |
| **TCPCLv4** | Component | [`PLAN-TCPCL-01`](../tcpclv4/docs/component_test_plan.md) | Session state machine via `duplex` harness. |
| **TCPCLv4** | Fuzz | [`FUZZ-TCPCL-01`](../tcpclv4/docs/fuzz_test_plan.md) | Protocol stream parsing and state machine robustness. |
| **TCPCLv4 Server** | System | [`PLAN-TCPCL-SERVER-01`](../tcpclv4-server/src/test_plan.md) | Application lifecycle, config, packaging. |
| **CLA Trait** | Integration | [`PLAN-CLA-01`](../bpa/docs/cla_integration_test_plan.md) | Generic Convergence Layer Trait verification. |
| **Service Trait** | Integration | [`PLAN-SVC-01`](../bpa/docs/service_integration_test_plan.md) | Generic Application Service Trait verification. |
| **Storage** | Integration | [`PLAN-STORE-01`](../bpa/docs/storage_integration_test_plan.md) | Generic Storage Trait verification. |
| **Storage** | Component | [`PLAN-SQLITE-01`](../sqlite-storage/docs/test_plan.md) | SQLite Metadata persistence. |
| **Storage** | Component | [`PLAN-LD-01`](../localdisk-storage/docs/test_plan.md) | Local Disk Bundle persistence. |
| **API** | Component | [`COMP-GRPC-CLIENT-01`](../proto/docs/component_test_plan.md) | Streaming gRPC interfaces (App/CLA). |
| **System** | System | [`PLAN-SERVER-01`](../bpa-server/docs/test_plan.md) | Application lifecycle, config, OpenTelemetry. |
| **System** | Interop | [`PLAN-INTEROP-01`](interop_test_plan.md) | Interoperability with ION/DTNME/etc. (REQ-20). |

## 3. Testing Levels (The Pyramid)

We adhere to a risk-based testing pyramid:

### 3.1 Level 1: Unit Testing (Automated)

* **Scope:** Individual Rust functions and modules (`hardy-cbor`, `hardy-bpv7`, `hardy-eid-patterns`).

* **Goal:** Verify logic correctness, memory safety, and parsing rules against RFCs.

* **Methodology:**
  * Tests are mapped explicitly to LLRs and RFC sections.
  * Strict separation of "Syntax/Parsing" tests from "BPA/Policy" tests.
  * Verification of "Factories" (Builders/Editors) to ensure API correctness.

* **Tooling:** `cargo test`, `tarpaulin` (coverage).

### 3.2 Level 2: Component Testing (CLI Driver & Harnesses)

* **Scope:** `hardy-bpv7` via CLI, `hardy-tcpcl` via duplex pipes, Storage via real DBs.

* **Goal:** Black-box verification of library logic and state machines without mocking internal implementation details.

* **Methodology:**
  * Shell-scripted test suites invoking the `bundle` binary.
  * Rust integration tests using `tokio` harnesses for async modules.

### 3.3 Level 3: Fuzzing & Security (Continuous)

* **Scope:** Public-facing parsers (`CBOR`, `Bundle`, `EID`, `Pattern`) and Async Pipelines (`BPA`).

* **Goal:** Identify crash-causing inputs, panics, memory vulnerabilities (OOM, Stack Overflow), and logic deadlocks.

* **Methodology:**
  * Dedicated Fuzz Plans for each target.
  * Continuous execution using `cargo-fuzz` (libFuzzer).
  * Sanitizer enabled (ASAN) runs to catch subtle memory violations.

### 3.4 Level 4: System Integration (GCP)

* **Scope:** Full system running in Docker/Kubernetes (`hardy-bpa-server`).

* **Goal:** Verify component interaction (BPA <-> Storage <-> TCPCL) and Interoperability.

* **Execution:** Pre-release verification in GCP Staging environment using `bping` and `tshark`.

## 4. Test Environment Architecture

### 4.1 Unit / CI Environment

* **Runner:** Standard Linux x64 (GitHub Actions).

* **Dependencies:** Rust Stable, OpenSSL.

### 4.2 System Test Environment (GCP)

To simulate a realistic cloud deployment, the following architecture is required:

* **Orchestration:** GKE Autopilot or Docker Compose.

* **Topology:**
  * `Node A` (Sender) -> `Node B` (Router/Hardy) -> `Node C` (Receiver).
  * Simulated Latency: `tc` (Traffic Control) or `toxiproxy` injected between nodes.

* **Storage Backend:**
  * S3 (Google CLoud Storage) for bundle persistence.
  * PostgreSQL for metadata persistence.

## 5. Tools & Frameworks

| Tool | Purpose | Source |
 | ----- | ----- | ----- |
| **cargo test** | Unit test runner | Rust Standard Lib |
| **cargo-fuzz** | Security/Fuzz testing | Rust Embedded |
| **hardy-bpv7-tools** | Component Test Driver | Internal (`bin/bundle`) |
| **bping** | DTN Traffic Generation | DTN Suite |
| **Wireshark** | Protocol Analysis | Standard (with BPv7 plugins) |
| **LocalStack** | AWS/GCP mocking | Docker Hub |
| **Grafana LGTM** | Trace/Metric Analysis | Docker Hub (`grafana/otel-lgtm`) |

## 6. Risk Management

| Risk | Impact | Mitigation Strategy |
| ----- | ----- | ----- |
| **Protocol Non-Compliance** | Interop failure with other BPv7 implementations. | Execute the full Interoperability Test Plan ([`PLAN-INTEROP-01`](interop_test_plan.md)) against multiple reference implementations (ION, DTNME, etc.). |
| **Parser Panics** | DoS vulnerability in production. | Enforce 100% fuzz coverage on all parsers (CBOR, Bundle, EID). |
| **Key Wrapping Failures** | Data loss or Security breach. | Specific Unit Tests for RFC 9173 Key Wrapping (AES-KW). |
| **Async Deadlocks** | Router hangs under load. | Property-based fuzzing of the BPA pipeline state machine. |

## 7. Performance Verification Strategy

Performance verification (REQ-13) is distributed across the testing hierarchy to ensure bottlenecks are identified early:

* **Component Level:** Micro-benchmarks for specific algorithms (e.g., Reassembly, Routing Table lookups) are defined in [`PLAN-BPA-01`](../bpa/src/component_test_plan.md).
* **System Level:** End-to-end throughput, latency, and storage scalability tests (10Gbps, 1TB capacity) are defined in [`PLAN-SERVER-01`](../bpa-server/docs/test_plan.md).
