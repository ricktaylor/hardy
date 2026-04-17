# Test Plan: Interoperability

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | System Interoperability |
| **Package** | `tests/interop` |
| **Requirements Ref** | [REQ-20](../../../docs/requirements.md#req-20-interoperability-with-existing-implementations) |
| **Test Suite ID** | PLAN-INTEROP-01 |
| **Version** | 1.2 |

## 1. Introduction

This document defines the interoperability testing strategy for Hardy against existing BPv7 implementations. The goal is to verify that Hardy can participate in a heterogeneous DTN network, correctly exchanging bundles via different convergence layer protocols.

All tests live in the `tests/interop/` directory. Each peer implementation has its own subdirectory with Docker configuration, start scripts, and test scripts. See the [README](../README.md) for run instructions.

## 2. Test Pattern

All implemented tests follow a bidirectional ping/echo pattern:

- **TEST 1 (Hardy → Peer)**: Hardy's `bp ping` sends bundles to the peer's echo service and measures round-trip time.
- **TEST 2 (Peer → Hardy)**: The peer's ping/send tool sends bundles to Hardy's echo service.

This pattern verifies session establishment, bundle encoding/decoding, CLA framing, and echo service compatibility in both directions.

## 3. Generic Test Suites

### Suite A: Transport Connectivity

*Objective: Verify Convergence Layer session establishment.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-01** | **Session Init** | 1. Start Peer (Passive).<br>2. Start Hardy (Active). | Session established; logs show successful handshake. |
| **IOP-02** | **Keepalive** | 1. Establish Session.<br>2. Idle for 2x Keepalive interval. | Session remains active. |
| **IOP-03** | **Graceful Close** | 1. Stop Hardy service. | Hardy sends termination signal; Peer logs "Session Terminated". |

### Suite B: Bundle Exchange

*Objective: Verify basic data transfer.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-04** | **Hardy to Peer** | 1. Hardy sends bundle to peer echo service. | Peer receives bundle; echo response returned. |
| **IOP-05** | **Peer to Hardy** | 1. Peer sends bundle to Hardy echo service. | Hardy receives bundle; echo response returned. |
| **IOP-06** | **Bidirectional Load** | 1. Both nodes send 100 bundles simultaneously. | All bundles delivered; no session drops. |

### Suite C: Administrative Logic

*Objective: Verify Status Reports and Extension Block handling.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-07** | **Status Report (Delivered)** | 1. Hardy sends bundle with "Report Delivery" flag. | Peer generates Status Report; Hardy receives it. |
| **IOP-08** | **Hop Count Exceeded** | 1. Hardy sends bundle with Hop Limit = 1 via relay. | Peer drops bundle; sends Hop Limit Exceeded report. |
| **IOP-09** | **Unknown Block** | 1. Hardy sends bundle with custom Extension Block (Critical=False). | Peer accepts bundle; preserves or ignores block. |

### Suite D: BPSec

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-10** | **BPSec Integrity** | 1. Hardy signs bundle (BIB-HMAC-SHA256).<br>2. Send to Peer (configured with same key). | Peer verifies signature and accepts bundle. |

### Suite E: Fragmentation

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-11** | **Reassembly** | 1. Peer fragments a large bundle into chunks.<br>2. Peer sends fragments to Hardy. | Hardy reassembles and delivers complete payload. |

## 4. Implementation Matrix

| Implementation | Transport | Suites Implemented | Status | Directory |
| :--- | :--- | :--- | :--- | :--- |
| **Hardy** | TCPCLv4 | A, B | Passing | `hardy/` |
| **dtn7-rs** | TCPCLv4 | A, B | Passing | `dtn7-rs/` |
| **HDTN** | TCPCLv4 | A, B | Passing | `HDTN/` |
| **DTNME** | TCPCLv4 | A, B | Passing | `DTNME/` |
| **ION** | STCP (via mtcp-cla) | A, B | Passing | `ION/` |
| **ud3tn** | MTCP (via mtcp-cla) | A, B | Passing | `ud3tn/` |
| **ESA BP** | STCP (via mtcp-cla) | A, B | Passing | `ESA-BP/` |
| **NASA cFS** | STCP (via mtcp-cla) | B | Passing | `NASA-cFS/` |

All 7 peer implementations are merged to main and passing 20/20 at 0% loss.

## 5. Test Topologies

### TCPCLv4 (dtn7-rs, HDTN, DTNME)

Uses Hardy's built-in TCPCLv4 CLA. Both nodes on Docker bridge network.

### STCP/MTCP (ION, ud3tn, ESA BP, NASA cFS)

Uses Hardy's standalone `mtcp-cla` binary (`tests/interop/mtcp/`), which provides STCP framing (4-byte length prefix) or MTCP framing (CBOR byte string). Both nodes on same host via `--network host`.

## 6. Coverage Boundary

### Covered by interop tests

| Area | What is verified |
| :--- | :--- |
| CLA session lifecycle | Handshake, keepalive, graceful close (IOP-01..03) |
| Bundle encoding/decoding | Wire format compatibility across implementations (IOP-04..05) |
| Echo service | Round-trip bundle exchange in both directions |
| CLA framing | TCPCLv4, STCP, and MTCP wire protocols |

### NOT covered — requires dedicated tests

| Area | Why not interop |
| :--- | :--- |
| Status Reports (IOP-07..08) | Requires multi-hop topology; not all peers support reporting |
| BPSec (IOP-10) | Requires shared key configuration; few peers support RFC 9173 |
| Fragmentation (IOP-11) | Requires peer-side fragmentation capability |
| Administrative records | Beyond ping/echo pattern |

## 7. Execution

See [README](../README.md) for detailed run instructions, Docker image management, and troubleshooting.

```sh
# Run all tests
./tests/interop/run_all.sh

# Run individual implementation test
./tests/interop/HDTN/test_hdtn_ping.sh
```
