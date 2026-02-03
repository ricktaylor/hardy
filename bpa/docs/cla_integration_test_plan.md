# Test Plan: CLA Integration (Generic Trait)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Convergence Layer Adapters (Transport) |
| **Module** | `hardy-bpa` |
| **Interface** | `crate::cla::Cla` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-3, REQ-5, REQ-6), `DTN-LLR_v1.1` (Section 3, Section 6) |
| **Test Suite ID** | PLAN-CLA-01 |

## 1. Introduction

This document details the integration testing strategy for implementations of the `Cla` trait. This trait abstracts the underlying transport protocols (TCP, UDP, File, etc.) from the BPA.

The tests defined here are intended to be run against **all** implementations of the trait (TCPCL, File, etc.) via a common harness.

## 2. Requirements Mapping

| ID | Requirement | Test Coverage |
| :--- | :--- | :--- |
| **REQ-3** | TCPCLv4 Compliance. | Verified by running suite against `hardy-tcpcl`. |
| **3.1.1** | Active Session Establishment. | Covered by **Suite D (Peer Management)**. |
| **6.1.3** | Forwarding Success API. | Covered by **Suite B (Forwarding)**. |

## 3. Test Suites

### Suite A: Lifecycle

*Objective: Verify the initialization and shutdown of the CLA.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **CLA-01** | **Register & Unregister** | 1. Instantiate CLA.<br>2. Call `on_register(sink)`.<br>3. Call `on_unregister()`. | 1. CLA initializes resources (sockets/files).<br>2. CLA cleans up resources. |
| **CLA-02** | **Self-Unregister** | 1. Register CLA.<br>2. Trigger fatal error (impl specific). | 1. CLA calls `sink.unregister()`. |

### Suite B: Forwarding

*Objective: Verify the CLA can transmit bundles to peers.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **CLA-03** | **Forward Success** | 1. Register CLA.<br>2. Call `forward(addr, bundle)`. | 1. Returns `Ok(Sent)`.<br>2. Data appears on wire/medium. |
| **CLA-04** | **Forward Failure** | 1. Call `forward` to unreachable address. | 1. Returns `Ok(NoNeighbour)` or `Err`. |
| **CLA-05** | **Queue Selection** | 1. Call `forward(queue=Some(0))`.<br>2. Call `forward(queue=None)`. | 1. CLA respects queue priority (if supported). |

### Suite C: Reception

*Objective: Verify the CLA correctly ingests data from the medium.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **CLA-06** | **Receive Bundle** | 1. Inject bundle data into medium (socket/file). | 1. CLA calls `sink.dispatch(bundle)`. |
| **CLA-07** | **Corrupt Data** | 1. Inject garbage data. | 1. CLA drops data or reports error (no crash). |

### Suite D: Peer Management

*Objective: Verify the CLA detects and reports peer connectivity.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **CLA-08** | **Peer Discovery** | 1. Connect a remote peer to the CLA. | 1. CLA calls `sink.add_peer(node_id, addr)`. |
| **CLA-09** | **Peer Loss** | 1. Disconnect remote peer. | 1. CLA calls `sink.remove_peer(node_id, addr)`. |

## 4. Execution Strategy

These tests are implemented as a generic harness that can be applied to any `Cla` implementation.

* **Harness File:** `tests/cla_harness.rs`
* **Command:** `cargo test --test cla_harness`
