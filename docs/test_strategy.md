# Test Strategy: Hardy DTN Router

| Document Info | Details |
| ----- | ----- |
| **Project** | Hardy DTN Router |
| **Repository** | `github.com/ricktaylor/hardy` |
| **Version** | 1.2 |

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
| **BPSec** | Unit | [`UTP-BPSEC-01`](../bpv7/docs/unit_test_plan_bpsec.md) | RFC 9172/3 Integrity & Confidentiality. |
| **EID Patterns** | Unit | [`UTP-PAT-01`](../eid-patterns/docs/unit_test_plan.md) | Draft-05 Pattern matching logic. |
| **EID Patterns** | Fuzz | [`FUZZ-PAT-01`](../eid-patterns/docs/fuzz_test_plan.md) | Pattern DSL parser robustness. |
| **BPA Logic** | Unit | [`UTP-BPA-01`](../bpa/docs/unit_test_plan.md) | BPA internal algorithms (Routing, Policy). |
| **BPA Logic** | Integration | [`PLAN-BPA-01`](../bpa/docs/component_test_plan.md) | Routing, Pipeline, Performance Benchmarks. |
| **BPA Pipeline** | Fuzz | [`FUZZ-BPA-01`](../bpa/docs/fuzz_test_plan.md) | Async pipeline stability and deadlocks. |
| **OpenTelemetry** | Unit | [`UTP-OTEL-01`](../otel/docs/unit_test_plan.md) | Metrics recorder bridge (gauge, counter, histogram). |
| **OpenTelemetry** | Component | [`COMP-OTEL-01`](../otel/docs/component_test_plan.md) | OTLP export verification (traces, metrics, logs). |
| **TCPCLv4** | Component | [`PLAN-TCPCL-01`](../tcpclv4/docs/component_test_plan.md) | Session state machine via `duplex` harness. |
| **TCPCLv4** | Fuzz | [`FUZZ-TCPCL-01`](../tcpclv4/docs/fuzz_test_plan.md) | Protocol stream parsing and state machine robustness. |
| **TCPCLv4 Server** | System | [`PLAN-TCPCL-SERVER-01`](../tcpclv4-server/docs/test_plan.md) | Application lifecycle, config, packaging. |
| **CLA Trait** | Integration | [`PLAN-CLA-01`](../bpa/docs/cla_integration_test_plan.md) | Generic Convergence Layer Trait verification. |
| **Service Trait** | Integration | [`PLAN-SVC-01`](../bpa/docs/service_integration_test_plan.md) | Generic Application Service Trait verification. |
| **Storage** | Integration | [`PLAN-STORE-01`](../tests/storage/docs/test_plan.md) | Generic Storage Trait verification. |
| **Storage** | Component | [`PLAN-SQLITE-01`](../sqlite-storage/docs/test_plan.md) | SQLite Metadata persistence. |
| **Storage** | Component | [`PLAN-LD-01`](../localdisk-storage/docs/test_plan.md) | Local Disk Bundle persistence. |
| **Storage** | Component | [`PLAN-PG-01`](../postgres-storage/docs/test_plan.md) | PostgreSQL Metadata persistence. |
| **Storage** | Component | [`PLAN-S3-01`](../s3-storage/docs/test_plan.md) | S3 Bundle persistence. |
| **API** | Component | [`COMP-GRPC-01`](../proto/docs/component_test_plan.md) | Streaming gRPC proxy interfaces (client & server). |
| **TVR** | Unit | [`UTP-TVR-01`](../tvr/docs/unit_test_plan.md) | Contact scheduling (cron, parser, scheduler). |
| **TVR** | Component | [`COMP-TVR-01`](../tvr/docs/component_test_plan.md) | gRPC session lifecycle, file hot-reload, system integration. |
| **Async** | Unit | [`UTP-ASYNC-01`](../async/docs/unit_test_plan.md) | TaskPool, sync primitives, cancellation. |
| **Echo Service** | Component | [`PLAN-ECHO-01`](../echo-service/docs/test_plan.md) | Bundle echo service diagnostics. |
| **IPN Legacy Filter** | Unit | [`UTP-IPN-LEGACY-01`](../ipn-legacy-filter/docs/unit_test_plan.md) | Legacy 2-element IPN EID encoding. |
| **Tools** | Component | [`PLAN-TOOLS-01`](../tools/docs/test_plan.md) | CLI tools (`bp ping`, bundle operations). |
| **System** | System | [`PLAN-SERVER-01`](../bpa-server/docs/test_plan.md) | Application lifecycle, config, OpenTelemetry. |
| **System** | Interop | [`PLAN-INTEROP-01`](../tests/interop/docs/test_plan.md) | Interoperability with ION/DTNME/etc. (REQ-20). |

## 3. Testing Levels

### 3.1 Unit Testing

* **Scope:** Individual Rust functions and modules across all library crates.
* **Goal:** Verify logic correctness, parsing rules against RFCs, and API contracts.
* **Methodology:** Tests are mapped to LLRs. Parsing/format tests are separated from BPA policy tests. Builder/Editor tests verify round-trip correctness.
* **Tooling:** `cargo test`, `cargo llvm-cov`.
* **Examples:** CBOR encoding/decoding, BPv7 bundle parsing, EID pattern matching, BPA routing logic, TVR cron/parser/scheduler, OTEL metrics bridge.

### 3.2 Component & Integration Testing

* **Scope:** Cross-module interactions verified through harnesses and CLI drivers.
* **Goal:** Black-box verification of state machines, storage backends, gRPC interfaces, and CLI tools.
* **Methodology:**
  * Shell-scripted test suites invoking the `bundle` and `cbor` CLIs.
  * Tokio-based harnesses for async modules (TCPCLv4 duplex, gRPC proxy).
  * Generic storage harness (`tests/storage/`) exercises CRUD, polling, and recovery across all backends.
  * `grpcurl`-driven session tests for TVR and proto crates.
* **Examples:** TCPCLv4 session negotiation, storage trait compliance, BPA pipeline end-to-end, TVR gRPC sessions and file hot-reload.

### 3.3 Fuzz Testing

* **Scope:** Parsers (CBOR, Bundle, EID string/CBOR, EID patterns), protocol streams (TCPCLv4 passive/active), and the BPA async pipeline.
* **Goal:** Identify panics, memory safety issues, and deadlocks from adversarial input.
* **Methodology:** Dedicated fuzz plans per target using `cargo fuzz` (libFuzzer). Corpus-based regression via CI. Coverage measured separately from unit tests.
* **Targets:** 11 fuzz binaries across 5 crates (cbor, bpv7, eid-patterns, bpa, tcpclv4).

### 3.4 System & Interoperability Testing

* **Scope:** Full system running in Docker (`hardy-bpa-server`, `hardy-tcpclv4-server`, `hardy-tvr`) with real storage and peer implementations.
* **Goal:** Verify component interaction and bidirectional bundle exchange with other BPv7 implementations.
* **Methodology:** Docker Compose topologies with `bp ping` for verification. Each peer runs in its own container connected via TCPCLv4, MTCP, or STCP.
* **Peers:** dtn7-rs, HDTN, DTNME, ud3tn, ION, ESA-BP, NASA cFS (7 implementations, all passing).

## 4. Test Environment Architecture

### 4.1 Unit / CI Environment

* **Runner:** Standard Linux x64 (GitHub Actions).
* **Dependencies:** Rust Stable, Rust Nightly (fuzz only), OpenSSL, Protobuf compiler.
* **Scope:** Unit tests, component tests, `cargo llvm-cov` coverage, `cargo fuzz` (corpus replay).

### 4.2 Local Docker Environment

Docker Compose is used for integration and interop testing with multiple nodes and storage backends.

* **Topology:** Hardy node(s) with echo service, peer implementation nodes (ION, HDTN, etc.) connected via TCPCLv4, MTCP, or STCP.
* **Storage backends:** SQLite + local filesystem (default), PostgreSQL + MinIO (full stack).
* **Observability:** Grafana LGTM stack for OpenTelemetry verification.

### 4.3 Interoperability Environment

Each peer implementation runs in its own Docker container alongside a Hardy node. Tests use `bp ping` to verify bidirectional bundle exchange.

* **Peers:** dtn7-rs, HDTN, DTNME, ud3tn (TCPCLv4/MTCP), ION, ESA-BP, NASA cFS (STCP via `hardy-mtcp-cla`).
* **Execution:** `tests/interop/run_all.sh` runs all peer tests and compares RTT.

## 5. Tools & Frameworks

| Tool | Purpose | Source |
| ----- | ----- | ----- |
| **cargo test** | Unit and integration test runner | Rust toolchain |
| **cargo fuzz** | Fuzz testing (libFuzzer) | [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) |
| **cargo llvm-cov** | Line coverage measurement (lcov) | [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) |
| **criterion** | Performance benchmarking | [criterion.rs](https://github.com/bheisler/criterion.rs) |
| **hardy-bpv7-tools** | Component test driver (`bundle` CLI) | Internal |
| **hardy-cbor-tools** | CBOR inspection and conversion (`cbor` CLI) | Internal |
| **hardy-tools** | Network diagnostics (`bp ping`) | Internal |
| **hardy-mtcp-cla** | MTCP/STCP interop CLA driver | Internal (`tests/interop/mtcp/`) |
| **grpcurl** | gRPC session testing (TVR, proto) | [grpcurl](https://github.com/fullstorydev/grpcurl) |
| **Grafana LGTM** | OpenTelemetry trace/metric analysis | Docker Hub (`grafana/otel-lgtm`) |

## 6. Risk Management

| Risk | Impact | Mitigation |
| ----- | ----- | ----- |
| **Protocol Non-Compliance** | Interop failure with other BPv7 implementations. | Interoperability verified against 7 implementations ([`PLAN-INTEROP-01`](../tests/interop/docs/test_plan.md)). |
| **Parser Panics** | DoS vulnerability in production. | Fuzz testing on all public-facing parsers (11 targets across 5 crates). |
| **Key Wrapping Failures** | Data loss or security breach. | Unit tests for RFC 9173 Key Wrapping (AES-KW, HMAC-SHA2). |
| **Async Deadlocks** | Router hangs under load. | BPA pipeline fuzz target exercises concurrent message processing. |
| **Storage Corruption** | Data loss after crash or restart. | Storage harness tests recovery and restart across all backends. |

## 7. Performance Verification

Performance verification (REQ-13) is distributed across the testing hierarchy:

* **Micro-benchmarks:** Criterion benchmarks in [`PLAN-BPA-01`](../bpa/docs/component_test_plan.md) measure bundle processing throughput (~8K bundles/sec sustained).
* **Interop RTT:** `tests/interop/run_all.sh` compares round-trip times across all peer implementations.
* **Scale targets** (REQ-13.2–13.5): 4GB reassembly, 1TB storage, 10Gbps TCPCLv4 — not yet tested, planned for full activity phase.
