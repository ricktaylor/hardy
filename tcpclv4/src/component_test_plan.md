# Component Test Plan: TCP Convergence Layer v4 (TCPCLv4)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Convergence Layer (Transport) |
| **Module** | `tcpclv4` |
| **Implements** | `hardy_bpa::cla::Cla` |
| **Parent Plan** | `hardy-bpa/tests/cla_integration_test_plan.md` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-3), `DTN-LLR_v1.1` (Section 3) |
| **Test Suite ID** | `PLAN-TCPCL-01` |

## 1. Introduction

This document details the testing strategy for the `tcpclv4` crate. This crate provides a concrete implementation of the `Cla` trait using the TCP Convergence Layer Protocol Version 4, as specified in **RFC 9174**.

## 2. Testing Strategy

The verification strategy is two-fold:

1. **Generic Trait Compliance:** The `tcpclv4` implementation is run against the generic test harness for the `Cla` trait (`PLAN-CLA-01`) to ensure it correctly interfaces with the BPA's routing and dispatch logic.
2. **Protocol Compliance:** Specific component tests are defined to verify the on-the-wire behavior of the TCPCLv4 state machine, including session management, data segmentation, and TLS. These tests use a `duplex` harness to simulate a peer and inspect the byte stream.

## 3. Generic Test Coverage

The following suites from the parent plan (`PLAN-CLA-01`) are executed against `tcpclv4` to verify its compliance with the `Cla` trait:

* **Suite A: Lifecycle** (Register/Unregister)
* **Suite B: Forwarding** (Forward Success/Failure)
* **Suite C: Reception** (Receive Bundle/Corrupt Data)
* **Suite D: Peer Management** (Peer Discovery/Loss)

## 4. Specific TCPCLv4 Tests

These tests verify the RFC 9174 protocol logic using a dedicated component test harness.

| Test ID | Scenario | Description | LLR Ref |
| :--- | :--- | :--- | :--- |
| **TCP-01** | **Active/Passive Handshake** | Verify the 3-way handshake (`CONTACT`, `SESS_INIT`, `SESS_ACK`) for both initiator and listener roles. | 3.1.1, 3.1.2 |
| **TCP-02** | **Session Parameters** | Verify that parameters from the `SESS_INIT` header (Node ID, Keepalive) are correctly parsed and applied. | 3.1.4, 3.1.5 |
| **TCP-03** | **Data Segmentation** | Verify a large bundle is correctly fragmented into multiple `XFER_SEGMENT` messages and reassembled by the peer. | N/A |
| **TCP-04** | **Keepalive** | Verify that `KEEPALIVE` messages are sent during idle periods and that the session is dropped if they are not acknowledged. | 3.1.10 |
| **TCP-05** | **TLS Handshake (Default)** | Verify that TLS is enabled by default and that a secure session is established using the provided certificates. | 3.1.7, 3.1.8 |
| **TCP-06** | **TLS Disabled** | Verify that if TLS is explicitly disabled in the configuration, the session is established in plaintext. | 3.1.8 |
| **TCP-07** | **Connection Pooling** | Verify that after a bundle is sent, the underlying TCP connection is returned to an idle pool and reused for a subsequent transfer. | 3.1.3 |
| **TCP-08** | **Protocol Error** | Send an invalid header (e.g., bad magic number in `CONTACT`) and verify the connection is terminated immediately. | N/A |
| **TCP-09** | **TLS Entity ID** | Verify the peer's certificate is validated against the expected DNS Name or Network Address. | 3.1.9 |
| **TCP-10** | **Session Extensions** | Verify that unknown (but valid) extension items in the `SESS_INIT` header are ignored and do not cause a handshake failure. | 3.1.6 |

## 5. Unit Test Coverage

*Scope: Internal logic verification without network I/O.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Message SerDes (UT-TCP-01)** | Verify encoding and decoding of all TCPCL message types. | `src/codec.rs` | Bytes of `SESS_INIT`, `XFER_SEGMENT`, etc. | Decoded structs match input / Encoded bytes match spec. |
| **Contact Header (UT-TCP-02)** | Verify validation of the magic string and version. | `src/session.rs` | `dtn!` + Version 4. | Handshake proceeds. |
| **Parameter Negotiation (UT-TCP-03)** | Verify negotiation of Keepalive and Segment Size. | `src/session.rs` | Local: 60s, Peer: 30s. | Negotiated: 30s (Min). |
| **Fragment Logic (UT-TCP-04)** | Verify splitting payload into segments. | `src/session.rs` | Payload: 1000B, MTU: 100B. | 10 `XFER_SEGMENT` messages sent. |
| **Reason Codes (UT-TCP-05)** | Verify mapping of internal errors to reason codes. | `src/session.rs` | `Error::StorageFull`. | `SESS_TERM` with `ResourceExhaustion`. |

## 6. Execution Strategy

* **Unit/Component Tests:** `cargo test -p hardy-tcpclv4`
* **Integration Tests:** `cargo test --test cla_harness` (via `hardy-bpa` harness)
