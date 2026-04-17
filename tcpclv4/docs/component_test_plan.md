# Component Test Plan: TCP Convergence Layer v4 (TCPCLv4)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Convergence Layer (Transport) |
| **Module** | `tcpclv4` |
| **Implements** | `hardy_bpa::cla::Cla` |
| **Parent Plan** | [`PLAN-CLA-01`](../../bpa/docs/cla_integration_test_plan.md) |
| **Requirements Ref** | [REQ-3](../../docs/requirements.md#req-3-full-compliance-with-rfc9174), [LLR 3.1.x](../../docs/requirements.md#tcpclv4-31) |
| **Test Suite ID** | `PLAN-TCPCL-01` |
| **Version** | 1.2 |

## 1. Introduction

This document details the testing strategy for the `tcpclv4` crate. This crate provides a concrete implementation of the `Cla` trait using the TCP Convergence Layer Protocol Version 4, as specified in **RFC 9174**.

## 2. Testing Strategy

The verification strategy combines three layers:

1. **Generic Trait Compliance:** The `tcpclv4` implementation is verified against the `Cla` trait contract ([`PLAN-CLA-01`](../../bpa/docs/cla_integration_test_plan.md)).
2. **Protocol Compliance:** Verified through interoperability testing ([`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md)) against 4 independent TCPCLv4 implementations (Hardy, dtn7-rs, HDTN, DTNME). This provides stronger verification than an in-process harness, as it exercises the full protocol stack including TCP, TLS, and session negotiation against real peers.
3. **Robustness:** Fuzz testing ([`FUZZ-TCPCL-01`](fuzz_test_plan.md)) with adversarial byte streams verifies that malformed input (including protocol errors like bad magic, invalid messages, and truncated frames) is handled without panics or hangs.

A `duplex`-based in-process harness was originally planned but is not needed — interop tests cover 9 of 10 component scenarios against real implementations, and fuzz tests cover the remaining scenario (TCP-08: protocol error handling) by verifying clean termination on arbitrary malformed input.

## 3. Generic Test Coverage

The following suites from the parent plan ([`PLAN-CLA-01`](../../bpa/docs/cla_integration_test_plan.md)) are executed against `tcpclv4` to verify its compliance with the `Cla` trait:

* **Suite A: Lifecycle** (Register/Unregister)
* **Suite B: Forwarding** (Forward Success/Failure)
* **Suite C: Reception** (Receive Bundle/Corrupt Data)
* **Suite D: Peer Management** (Peer Discovery/Loss)

## 4. Specific TCPCLv4 Tests

These scenarios verify RFC 9174 protocol logic. All are covered by interop testing or fuzz testing — no dedicated component test harness is required.

| Test ID | Scenario | LLR Ref | Covered By |
| :--- | :--- | :--- | :--- |
| **TCP-01** | **Active/Passive Handshake** | 3.1.1, 3.1.2 | Interop: every test exercises both roles |
| **TCP-02** | **Session Parameters** | 3.1.4, 3.1.5 | Interop: node ID + parameter exchange with all peers |
| **TCP-03** | **Data Segmentation** | — | Interop: every bundle transfer exercises segmentation |
| **TCP-04** | **Keepalive** | 3.1.10 | Interop: long-running tests |
| **TCP-05** | **TLS Handshake (Default)** | 3.1.7, 3.1.8 | Interop: TLS-capable peers |
| **TCP-06** | **TLS Disabled** | 3.1.8 | Interop: `--no-tls` configuration |
| **TCP-07** | **Connection Pooling** | 3.1.3 | Interop: multi-ping connection reuse |
| **TCP-08** | **Protocol Error** | — | Fuzz: adversarial byte streams (bad magic, invalid messages, truncated frames) |
| **TCP-09** | **TLS Entity ID** | 3.1.9 | Interop: TLS peers with certificate validation |
| **TCP-10** | **Session Extensions** | 3.1.6 | Interop: passively exercised (peers send extensions) |

## 5. Unit Test Coverage

*Scope: Internal logic verification without network I/O.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Message SerDes (UT-TCP-01)** | Verify encoding and decoding of all TCPCL message types. | `src/codec.rs` | Bytes of `SESS_INIT`, `XFER_SEGMENT`, etc. | Decoded structs match input / Encoded bytes match spec. |
| **Contact Header (UT-TCP-02)** | Verify validation of the magic string and version. | `src/connect.rs` | `dtn!` + Version 4. | Handshake proceeds. |
| **Parameter Negotiation (UT-TCP-03)** | Verify negotiation of Keepalive and Segment Size. | `src/session.rs` | Local: 60s, Peer: 30s. | Negotiated: 30s (Min). |
| **Fragment Logic (UT-TCP-04)** | Verify splitting payload into segments. | `src/session.rs` | Payload: 1000B, MTU: 100B. | 10 `XFER_SEGMENT` messages sent. |
| **Reason Codes (UT-TCP-05)** | Verify mapping of internal errors to reason codes. | `src/session.rs` | `Error::StorageFull`. | `SESS_TERM` with `ResourceExhaustion`. |

## 6. Connection Scaling Tests

*Objective: Verify TCPCL performance with many concurrent connections. These tests are scoped for the Full Activity phase.*

| Test ID | Scenario | Procedure | Pass Criteria |
| :--- | :--- | :--- | :--- |
| **TCPCL-SCALE-01** | **Concurrent Sessions (100)** | Establish 100 simultaneous TCPCL connections. Send bundles on each. | All connections stable. Per-connection throughput > 10% of single-connection baseline. |
| **TCPCL-SCALE-02** | **Concurrent Sessions (1000)** | Establish 1000 simultaneous TCPCL connections. Measure system resource usage. | All connections accepted. CPU < 80%. Memory < configured limit. |
| **TCPCL-SCALE-03** | **Connection Churn** | Continuously connect/disconnect at 100 conn/sec for 10 minutes. | No connection failures. No resource leaks. |
| **TCPCL-SCALE-04** | **TLS Handshake Throughput** | Measure TLS session establishment rate. | > 500 TLS handshakes/sec (single core). |

## 7. Deprecated: `duplex` Test Harness

The original v1.0 plan proposed a `tokio::io::duplex`-based in-process harness to simulate a peer and inspect the byte stream for TCP-01 through TCP-10. This has been superseded by:

- **Interop tests** (TCP-01..07, 09, 10): testing against 4 real TCPCLv4 implementations provides stronger verification than an in-process simulation, as it exercises the full TCP/TLS stack and real SESS_INIT negotiation.
- **Fuzz tests** (TCP-08): adversarial byte streams exercise the same error-handling paths that crafted protocol frames would, with broader input coverage.

The duplex harness is not planned for implementation.

## 8. Execution Strategy

* **Unit Tests:** `cargo test -p hardy-tcpclv4`
* **Interop Tests:** `./tests/interop/run_all.sh`
