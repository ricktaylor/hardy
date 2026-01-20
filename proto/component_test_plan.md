# Component Test Plan: gRPC Client Proxies

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | gRPC Client Implementation |
| **Module** | `hardy-proto` (Client Proxies) |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-18) |
| **Test Suite ID** | COMP-GRPC-CLIENT-01 |

## 1. Introduction

This document details the component testing strategy for the client-side implementations of the Hardy gRPC interfaces. These clients (Proxies) are responsible for abstracting the gRPC streaming complexity and providing a clean Rust API for Applications and CLAs to communicate with the BPA.

**Scope:**

* **Application Proxy:** Verification of `ApplicationClient` logic.
* **CLA Proxy:** Verification of `ClaClient` logic.
* **Protocol Compliance:** Ensuring Rust method calls translate to correct Protobuf messages.
* **Stream Handling:** Verifying correct handling of incoming server streams (Push messages).

**Out of Scope:**

* Server-side logic (BPA implementation).
* Network transport reliability (TCP/IP).

## 2. Test Architecture

The tests utilize a **Mock Server** approach:

1. **Mock Server:** A lightweight `tonic` server runs in a background thread/task. It asserts expectations on received requests and injects specific responses.
2. **Client Under Test:** The actual `hardy-proto` client connects to the mock server via a local loopback address.

## 3. Test Suites

### Suite 1: Application Client Proxy

*Objective: Verify the `ApplicationClient` correctly maps Rust types to `application.proto` messages.*

| Test ID | Scenario | Client Action (Rust) | Mock Server Assertion | Status |
| ----- | ----- | ----- | ----- | ----- |
| **APP-CLI-01** | **Registration (IPN)** | Call `register_ipn(101)` | Receives `RegisterApplicationRequest { service_id: { ipn: 101 } }`.<br>Replies `RegisterApplicationResponse { endpoint_id: "ipn:1.101" }`. | Implemented |
| **APP-CLI-02** | **Registration (DTN)** | Call `register_dtn("sensor")` | Receives `RegisterApplicationRequest { service_id: { dtn: "sensor" } }`. | Implemented |
| **APP-CLI-03** | **Send Bundle** | Call `send("ipn:2.1", payload, lifetime)` | Receives `SendRequest { destination: "ipn:2.1", payload: ..., lifetime: ... }`.<br>Replies `SendResponse`. | Implemented |
| **APP-CLI-04** | **Receive Bundle** | Await `next_message()` | Server injects `ReceiveBundleRequest`.<br>Client yields `AppMsg::ReceiveBundle(...)`. | Implemented |
| **APP-CLI-05** | **Status Notification** | Await `next_message()` | Server injects `StatusNotifyRequest`.<br>Client yields `AppMsg::StatusNotify(...)`. | Implemented |
| **APP-CLI-06** | **Cancel Transmission** | Call `cancel(bundle_id)` | Receives `CancelRequest { bundle_id: ... }`.<br>Replies `CancelResponse`. | Implemented |

### Suite 2: CLA Client Proxy

*Objective: Verify the `ClaClient` correctly maps Rust types to `cla.proto` messages.*

| Test ID | Scenario | Client Action (Rust) | Mock Server Assertion | Status |
| ----- | ----- | ----- | ----- | ----- |
| **CLA-CLI-01** | **Registration** | Call `register("tcp-1", ClaAddressType::Tcp)` | Receives `RegisterClaRequest { name: "tcp-1", address_type: TCP }`.<br>Replies `RegisterClaResponse`. | Implemented |
| **CLA-CLI-02** | **Dispatch Bundle** | Call `dispatch(bundle_bytes)` | Receives `DispatchBundleRequest { bundle: ... }`.<br>Replies `DispatchBundleResponse`. | Implemented |
| **CLA-CLI-03** | **Forward Bundle** | Await `next_message()` | Server injects `ForwardBundleRequest`.<br>Client yields `ClaMsg::ForwardBundle(...)`. | Implemented |
| **CLA-CLI-04** | **Add Peer** | Call `add_peer(node_id, address)` | Receives `AddPeerRequest`.<br>Replies `AddPeerResponse`. | Implemented |
| **CLA-CLI-05** | **Remove Peer** | Call `remove_peer(node_id)` | Receives `RemovePeerRequest`.<br>Replies `RemovePeerResponse`. | Implemented |

### Suite 3: Error Handling & Lifecycle

*Objective: Verify the client handles protocol violations and connection issues gracefully.*

| Test ID | Scenario | Setup | Expected Behavior | Status |
| ----- | ----- | ----- | ----- | ----- |
| **ERR-CLI-01** | **Connection Refused** | Server is offline. | Client `connect()` returns `Err(TransportError)`. | Implemented |
| **ERR-CLI-02** | **Premature Stream End** | Server closes stream immediately after handshake. | Client `next_message()` returns `None` (Stream Closed). | Implemented |
| **ERR-CLI-03** | **Protocol Violation** | Server sends `RegisterApplicationResponse` *twice*. | Client logic (if stateful) should handle or ignore, ensuring no panic. | Implemented |
| **ERR-CLI-04** | **Invalid Message Sequence** | Server sends `ReceiveBundleRequest` before Registration completes. | Client should return error or drop message depending on strictness. | Implemented |

## 4. Execution Strategy

These tests are implemented as integration tests within the `hardy-proto` package.

* **Implementation File:** `tests/client_tests.rs`
* **Command:** `cargo test -p hardy-proto --test client_tests`
* **Dependencies:** `tokio`, `tonic`, `proptest` (optional for fuzzing inputs).
