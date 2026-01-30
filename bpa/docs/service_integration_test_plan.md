# Test Plan: Service Integration (Generic Trait)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Application Interface |
| **Module** | `hardy-bpa` |
| **Interface** | `crate::service::Service` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-6, REQ-17, REQ-18), `DTN-LLR_v1.1` (Section 6) |
| **Test Suite ID** | PLAN-SVC-01 |

## 1. Introduction

This document details the integration testing strategy for implementations of the `Service` trait. This trait abstracts the application layer from the BPA, allowing various application bindings (Native, gRPC, WASM, etc.) to be plugged in.

The tests defined here are intended to be run against **all** implementations of the trait via a common harness.

## 2. Requirements Mapping

| ID | Requirement | Test Coverage |
| :--- | :--- | :--- |
| **REQ-18** | SDK/API Documentation & Examples. | Verified by running suite against `hardy-proto` (gRPC) and native examples. |

## 3. Test Suites

### Suite A: Lifecycle

*Objective: Verify the initialization and shutdown of the Service.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **SVC-01** | **Register & Unregister** | 1. Instantiate Service.<br>2. Call `on_register(source, sink)`.<br>3. Call `on_unregister()`. | 1. Service initializes.<br>2. Service cleans up resources. |
| **SVC-02** | **Sink Unregister** | 1. Register Service.<br>2. Service calls `sink.unregister()`. | 1. BPA receives unregister request. |

### Suite B: Reception & Notifications

*Objective: Verify the Service correctly handles incoming data from the BPA.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **SVC-03** | **Receive Bundle** | 1. BPA calls `on_receive(src, expiry, ack, payload)`. | 1. Service processes payload.<br>2. Service acknowledges (if applicable). |
| **SVC-04** | **Receive Status** | 1. BPA calls `on_status_notify(id, from, kind, reason, time)`. | 1. Service correlates ID with sent bundle.<br>2. Service handles status update. |

### Suite C: Transmission

*Objective: Verify the Service can transmit bundles via the Sink.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **SVC-05** | **Send Bundle** | 1. Service calls `sink.send(dest, payload, ttl, opts)`. | 1. Returns `Ok(bundle_id)`.<br>2. BPA accepts bundle for routing. |
| **SVC-06** | **Send Invalid** | 1. Service calls `sink.send` with invalid EID. | 1. Returns `Err(InvalidDestination)`. |
| **SVC-07** | **Cancel Bundle** | 1. Service calls `sink.send`.<br>2. Service calls `sink.cancel(bundle_id)`. | 1. Returns `Ok(true)`.<br>2. BPA aborts transmission. |

### Suite D: Error Handling

*Objective: Verify robustness against disconnected sinks.*

| Test ID | Scenario | Steps | Expected Result |
| :--- | :--- | :--- | :--- |
| **SVC-08** | **Disconnected Sink** | 1. Unregister Service.<br>2. Service calls `sink.send()`. | 1. Returns `Err(Disconnected)` or similar. |

## 4. Execution Strategy

These tests are implemented as a generic harness that can be applied to any `Service` implementation.

* **Harness File:** `tests/service_harness.rs`
* **Command:** `cargo test --test service_harness`
