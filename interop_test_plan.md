# Test Plan: Interoperability (Interop)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | System Interoperability |
| **Module** | `hardy-bpa-server` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-20) |
| **Test Suite ID** | PLAN-INTEROP-01 |

## 1. Introduction

This document details the strategy for verifying interoperability between **Hardy** and existing, compliant BPv7 implementations.

The goal is to ensure that Hardy can participate in a heterogeneous DTN network, correctly exchanging bundles, status reports, and routing information.

## 2. Generic Test Suites

The following suites define the *behavior* to be tested, independent of the specific transport or peer implementation.

### Suite A: Transport Connectivity

*Objective: Verify the Convergence Layer (CL) session establishment.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-01** | **Session Init** | 1. Start Peer (Passive).<br>2. Start Hardy (Active). | 1. Hardy connects to Peer.<br>2. Session established.<br>3. Logs show successful handshake. |
| **IOP-02** | **Keepalive** | 1. Establish Session.<br>2. Idle for 2x Keepalive interval. | 1. Session remains active.<br>2. Keepalives observed (if supported by CL). |
| **IOP-03** | **Graceful Close** | 1. Stop Hardy service. | 1. Hardy sends termination signal.<br>2. Peer logs "Session Terminated". |

### Suite B: Bundle Exchange

*Objective: Verify basic data transfer.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-04** | **Hardy to Peer** | 1. Hardy sends bundle to `ipn:2.1`. | 1. Peer receives bundle.<br>2. Peer utility prints payload. |
| **IOP-05** | **Peer to Hardy** | 1. Peer sends bundle to `ipn:1.1`. | 1. Hardy receives bundle.<br>2. Hardy delivers to registered application. |
| **IOP-06** | **Bidirectional Load** | 1. Both nodes send 100 bundles simultaneously. | 1. All bundles delivered.<br>2. No session drops or protocol errors. |

### Suite C: Administrative Logic

*Objective: Verify Status Reports and Extension Block handling.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-07** | **Status Report (Delivered)** | 1. Hardy sends bundle to `ipn:2.1` with "Report Delivery" flag. | 1. Peer accepts bundle.<br>2. Peer generates Status Report.<br>3. Hardy receives Report (Reason: Delivered). |
| **IOP-08** | **Hop Count Exceeded** | 1. Hardy sends bundle to `ipn:2.1` with Hop Limit = 1.<br>2. Configure Peer to forward to Node C (Simulated). | 1. Peer drops bundle (Hop Count Exceeded).<br>2. Peer sends Status Report (Reason: Hop Limit Exceeded). |
| **IOP-09** | **Unknown Block** | 1. Hardy sends bundle with custom Extension Block (Critical=False). | 1. Peer accepts bundle.<br>2. Peer preserves or ignores block (does not drop bundle). |

### Suite D: BPSec

*Objective: Verify security protocol interactions.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-10** | **BPSec Integrity** | 1. Hardy signs bundle (BIB-HMAC-SHA256).<br>2. Send to Peer (configured with same key). | 1. Peer verifies signature.<br>2. Peer accepts bundle. |

### Suite E: Fragmentation

*Objective: Verify reassembly of fragmented bundles.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-11** | **Reassembly** | 1. Peer fragments a large bundle (1MB) into 100KB chunks.<br>2. Peer sends fragments to Hardy. | 1. Hardy accepts fragments.<br>2. Hardy reassembles and delivers 1MB payload. |

## 3. Implementation Matrix

This matrix defines which implementations are tested and which suites are applicable based on their capabilities.

| Implementation | Version | Repository | Transport | Suites Covered | Notes |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **NASA ION** | 4.1.2+ | `github.com/nasa-jpl/ION-DTN` | File (Shared Vol) | B, C, D, E | ION lacks TCPCLv4. Use `file-cla` bridge via Docker volumes. |
| **DTNME** | 1.3.2 | `github.com/nasa/DTNME` | TCPCLv4 | A, B, C, E | Full TCPCLv4 support. BPSec not explicitly listed. |
| **HDTN** | 2.0.0 | `github.com/nasa/HDTN` | TCPCLv4 | A, B, D | High-rate C++ impl. No BPv7 Status Reports or Fragmentation. |
| **µD3TN** | 0.14.5 | `gitlab.com/d3tn/ud3tn` | File (Shared Vol) | B, C | No BPSec. Supports TCPCLv3 (not v4). Use AAP bridge. |
| **dtn7-rs** | 0.21.0 | `github.com/dtn7/dtn7-rs` | TCPCLv4 | A, B, C, E | Rust DTN daemon. BPSec not explicitly listed. |
| **ESA BP** | TBD | ESA Internal | File (Shared Vol) | B, C, D | ESA reference implementation (CCSDS 734.20-O-1 Annex 14). Contact ESA for access and version details. |

## 4. Test Topologies

Tests are executed using a containerized environment (Docker Compose).

### Topology 1: TCPCLv4 (Standard)

* **Node A (Hardy):** `ipn:1.0` (BPA), `ipn:1.1` (Sender/Receiver).
* **Node B (Peer):** `ipn:2.0` (BPA), `ipn:2.1` (Sender/Receiver).
* **Link:** TCPCLv4 over Docker bridge (`hardy:4556` <-> `peer:4556`).

### Topology 2: File Bridge (Compatibility)

Used for implementations that do not support TCPCLv4 (e.g., ION).

* **Node A (Hardy):** Configured with `file-cla` watching `/shared/to_hardy`.
* **Node B (Peer):** Configured (or adapted via bridge) to write bundles to `/shared/to_hardy` and read from `/shared/from_hardy`.
* **Link:** Shared Docker Volume mounted at `/shared`.

## 5. Execution Strategy

The test harness is external (Python/Bash) wrapping Docker commands.

**Prerequisites:**

1. Build `hardy-bpa-server` Docker image.
2. Pull or Build Peer Docker images (e.g., `amsat/ion-dtn`, `hdtn_ubuntu`).
3. Create `docker-compose.yml` defining the network and shared volumes.

**Command:**

```bash
./tests/interop/run_suite.sh --impl <ion|dtnme|hdtn|dtn7rs> --topology <tcp|file>
```
