# Test Plan: Interoperability (Interop)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | System Interoperability |
| **Module** | `hardy-bpa-server` |
| **Requirements Ref** | [REQ-20](requirements.md#req-20-interoperability-with-existing-implementations) |
| **Test Suite ID** | PLAN-INTEROP-01 |

## 1. Introduction

This document details the strategy for verifying interoperability between **Hardy** and existing, compliant BPv7 implementations.

The goal is to ensure that Hardy can participate in a heterogeneous DTN network, correctly exchanging bundles, status reports, and routing information.

## 2. Generic Test Suites

The following suites define the *behavior* to be tested, independent of the specific transport or peer implementation.

### Suite A: Transport Connectivity

*Objective: Verify the Convergence Layer (CL) session establishment.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-01** | **Session Init** | 1. Start Peer (Passive).<br>2. Start Hardy (Active). | 1. Hardy connects to Peer.<br>2. Session established.<br>3. Logs show successful handshake. |
| **IOP-02** | **Keepalive** | 1. Establish Session.<br>2. Idle for 2x Keepalive interval. | 1. Session remains active.<br>2. Keepalives observed (if supported by CL). |
| **IOP-03** | **Graceful Close** | 1. Stop Hardy service. | 1. Hardy sends termination signal.<br>2. Peer logs "Session Terminated". |

### Suite B: Bundle Exchange

*Objective: Verify basic data transfer.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-04** | **Hardy to Peer** | 1. Hardy sends bundle to `ipn:2.1`. | 1. Peer receives bundle.<br>2. Peer utility prints payload. |
| **IOP-05** | **Peer to Hardy** | 1. Peer sends bundle to `ipn:1.1`. | 1. Hardy receives bundle.<br>2. Hardy delivers to registered application. |
| **IOP-06** | **Bidirectional Load** | 1. Both nodes send 100 bundles simultaneously. | 1. All bundles delivered.<br>2. No session drops or protocol errors. |

### Suite C: Administrative Logic

*Objective: Verify Status Reports and Extension Block handling.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-07** | **Status Report (Delivered)** | 1. Hardy sends bundle to `ipn:2.1` with "Report Delivery" flag. | 1. Peer accepts bundle.<br>2. Peer generates Status Report.<br>3. Hardy receives Report (Reason: Delivered). |
| **IOP-08** | **Hop Count Exceeded** | 1. Hardy sends bundle to `ipn:2.1` with Hop Limit = 1.<br>2. Configure Peer to forward to Node C (Simulated). | 1. Peer drops bundle (Hop Count Exceeded).<br>2. Peer sends Status Report (Reason: Hop Limit Exceeded). |
| **IOP-09** | **Unknown Block** | 1. Hardy sends bundle with custom Extension Block (Critical=False). | 1. Peer accepts bundle.<br>2. Peer preserves or ignores block (does not drop bundle). |

### Suite D: BPSec

*Objective: Verify security protocol interactions.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-10** | **BPSec Integrity** | 1. Hardy signs bundle (BIB-HMAC-SHA256).<br>2. Send to Peer (configured with same key). | 1. Peer verifies signature.<br>2. Peer accepts bundle. |

### Suite E: Fragmentation

*Objective: Verify reassembly of fragmented bundles.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **IOP-11** | **Reassembly** | 1. Peer fragments a large bundle (1MB) into 100KB chunks.<br>2. Peer sends fragments to Hardy. | 1. Hardy accepts fragments.<br>2. Hardy reassembles and delivers 1MB payload. |

## 3. Implementation Matrix

This matrix defines which implementations are tested and which suites are applicable based on their capabilities.

| Implementation | Version | Repository | Transport | Suites Covered | Status | Notes |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **dtn7-rs** | 0.21.0 | `github.com/dtn7/dtn7-rs` | TCPCLv4 | A, B, C, E | ✅ A, B | Ping/echo tests implemented. See `tests/interop/dtn7-rs/`. |
| **HDTN** | 2.0.0 | `github.com/nasa/HDTN` | TCPCLv4 | A, B, D | ✅ A, B | Ping/echo tests implemented. See `tests/interop/HDTN/`. |
| **DTNME** | 1.3.2 | `github.com/nasa/DTNME` | TCPCLv4 | A, B, C, E | ✅ A, B | Ping/echo tests implemented. See `tests/interop/DTNME/`. |
| **NASA ION** | 4.1.2+ | `github.com/nasa-jpl/ION-DTN` | STCP (via mtcp-cla) | B, C, D, E | ✅ A, B | Ping/echo tests implemented. See `tests/interop/ION/`. |
| **µD3TN** | 0.14.5 | `gitlab.com/d3tn/ud3tn` | STCP (via mtcp-cla) | B, C | ✅ A, B | AAP2 protocol. Not yet merged to main. |
| **ESA BP** | 1.0 | ESA Internal | STCP (via mtcp-cla) | B, C, D | ✅ A, B | STCP CLE interop. Not yet merged to main. See `tests/interop/ESA-BP/`. |
| **NASA cFS** | TBD | NASA Internal | STCP (via mtcp-cla) | B | ✅ B | Not yet merged to main. |

## 4. Test Topologies

Tests are executed using a containerized environment (Docker Compose).

### Topology 1: TCPCLv4 (Standard)

* **Node A (Hardy):** `ipn:1.0` (BPA), `ipn:1.1` (Sender/Receiver).
* **Node B (Peer):** `ipn:2.0` (BPA), `ipn:2.1` (Sender/Receiver).
* **Link:** TCPCLv4 over Docker bridge (`hardy:4556` <-> `peer:4556`).

### Topology 2: STCP (Compatibility)

Used for implementations that do not support TCPCLv4 (e.g., ION, ESA-BP). Hardy's `mtcp-cla` binary provides STCP framing (4-byte length prefix) over TCP.

* **Node A (Hardy):** `ipn:1.0`, with `mtcp-cla` in STCP mode connecting to peer.
* **Node B (Peer):** Configured with native STCP/MTCP support.
* **Link:** STCP over `127.0.0.1` (both nodes on same host via `--network host`).

## 5. Execution Strategy

### Implemented Tests

Ping/echo tests are implemented for all 7 peer implementations. Tests on main use TCPCLv4 (dtn7-rs, HDTN, DTNME) or STCP via the `mtcp-cla` binary (ION). Tests on branches add ud3tn, ESA-BP, and cFS.

```bash
# Run all main-branch tests with benchmark comparison
./tests/interop/benchmark.sh [--skip-build] [--count N]

# Run individual implementation tests (main branch)
./tests/interop/hardy/test_hardy_ping.sh      # Hardy-to-Hardy baseline
./tests/interop/dtn7-rs/test_dtn7rs_ping.sh   # Hardy <-> dtn7-rs
./tests/interop/HDTN/test_hdtn_ping.sh        # Hardy <-> HDTN
./tests/interop/DTNME/test_dtnme_ping.sh      # Hardy <-> DTNME
./tests/interop/ION/test_ion_ping.sh          # Hardy <-> ION

# On feature branches
./tests/interop/ESA-BP/test_esa_bp_ping.sh    # Hardy <-> ESA-BP
```

**Prerequisites:**

1. Docker installed and running.
2. Hardy tools and bpa-server built (scripts build automatically, or use `--skip-build`).
3. For STCP-based tests: `mtcp-cla` binary built separately (`tests/interop/mtcp/`, workspace-excluded).

Docker images for peer implementations are built automatically on first run from Dockerfiles in each test directory.

### Planned: Full Suite Runner

For complete test coverage (Suites C, D, E):

```bash
./tests/interop/run_suite.sh --impl <ion|dtnme|hdtn|dtn7rs|esa-bp> --topology <tcp|stcp>
```

This will require:

1. Docker Compose configuration for multi-node topologies.
2. Extended test scripts for Status Reports, BPSec, and Fragmentation.
