# Component Test Plan: gRPC Proxies

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | gRPC Proxy Implementation |
| **Module** | `hardy-proto` (Client & Server Proxies) |
| **Requirements Ref** | [REQ-18](../../docs/requirements.md#req-18-comprehensive-technical-documentation-and-examples) |
| **Test Suite ID** | COMP-GRPC-01 |

## 1. Introduction

This document details the component testing strategy for the Hardy gRPC proxy interfaces. These proxies are responsible for abstracting the gRPC streaming complexity and providing transparent local/remote operation for CLAs, Services, Applications, and Routing Agents communicating with the BPA.

**Scope:**

* **Client Proxies:** Verification that client-side Sink implementations correctly translate Rust trait calls to Protobuf messages and deliver callbacks to trait implementations.
* **Server Proxies:** Verification that server-side handler implementations correctly proxy trait calls to remote components and manage lifecycle.
* **Protocol Compliance:** Ensuring Rust method calls translate to correct Protobuf messages.
* **Stream Handling:** Verifying correct handling of bidirectional streams, including graceful close.
* **Unregistration & Shutdown:** Verifying that stream close triggers correct cleanup on both sides for all shutdown scenarios.

**Out of Scope:**

* BPA-internal logic (RIB, dispatcher, storage).
* Network transport reliability (TCP/IP).

## 2. Test Architecture

The tests utilize a **Mock Server** approach:

1. **Mock Server:** A lightweight `tonic` server runs in a background thread/task. It asserts expectations on received requests and injects specific responses.
2. **Client Under Test:** The actual `hardy-proto` client connects to the mock server via a local loopback address.

## 3. Test Suites

### Suite 1: Application Client Proxy

*Objective: Verify the `ApplicationClient` correctly maps Rust types to `service.proto` messages (Application RPC).*

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

### Suite 3: Service Client Proxy

*Objective: Verify the `ServiceClient` correctly maps Rust types to `service.proto` messages (Service RPC).*

| Test ID | Scenario | Client Action (Rust) | Mock Server Assertion | Status |
| ----- | ----- | ----- | ----- | ----- |
| **SVC-CLI-01** | **Registration (IPN)** | Call `register_service(Some(Ipn(42)))` | Receives `RegisterRequest { service_id: { ipn: 42 } }`.<br>Replies `RegisterResponse { endpoint_id: "ipn:1.42" }`. | Not implemented |
| **SVC-CLI-02** | **Send Raw Bundle** | Call `sink.send(bundle_bytes)` | Receives `ServiceSendRequest { data: ... }`.<br>Replies `SendResponse { bundle_id }`. | Not implemented |
| **SVC-CLI-03** | **Receive Raw Bundle** | Server injects `ServiceReceiveRequest`. | Client trait receives `on_receive(data, expiry)`. | Not implemented |
| **SVC-CLI-04** | **Status Notification** | Server injects `StatusNotifyRequest`. | Client trait receives `on_status_notify(...)`. | Not implemented |
| **SVC-CLI-05** | **Cancel Transmission** | Call `sink.cancel(bundle_id)` | Receives `CancelRequest { bundle_id }`.<br>Replies `CancelResponse { cancelled }`. | Not implemented |

### Suite 4: Routing Agent Client Proxy

*Objective: Verify the Routing Agent client correctly maps Rust types to `routing.proto` messages.*

| Test ID | Scenario | Client Action (Rust) | Mock Server Assertion | Status |
| ----- | ----- | ----- | ----- | ----- |
| **RTE-CLI-01** | **Registration** | Call `register_routing_agent("tvr", agent)` | Receives `RegisterRoutingAgentRequest { name: "tvr" }`.<br>Replies `RegisterRoutingAgentResponse { node_ids }`. Agent receives `on_register` with sink and node IDs. | Not implemented |
| **RTE-CLI-02** | **Add Route** | Call `sink.add_route(pattern, action, priority)` | Receives `AddRouteRequest { pattern, action, priority }`.<br>Replies `AddRouteResponse { added: true }`. | Not implemented |
| **RTE-CLI-03** | **Remove Route** | Call `sink.remove_route(pattern, action, priority)` | Receives `RemoveRouteRequest { pattern, action, priority }`.<br>Replies `RemoveRouteResponse { removed: true }`. | Not implemented |

### Suite 5: Error Handling & Lifecycle

*Objective: Verify the client handles protocol violations and connection issues gracefully.*

| Test ID | Scenario | Setup | Expected Behavior | Status |
| ----- | ----- | ----- | ----- | ----- |
| **ERR-CLI-01** | **Connection Refused** | Server is offline. | Client `connect()` returns `Err(TransportError)`. | Implemented |
| **ERR-CLI-02** | **Premature Stream End** | Server closes stream immediately after handshake. | Client `on_close` fires, delivering synthetic `on_unregister()` to trait impl. | Implemented |
| **ERR-CLI-03** | **Protocol Violation** | Server sends `RegisterApplicationResponse` *twice*. | Client logic (if stateful) should handle or ignore, ensuring no panic. | Implemented |
| **ERR-CLI-04** | **Invalid Message Sequence** | Server sends `ReceiveBundleRequest` before Registration completes. | Client should return error or drop message depending on strictness. | Implemented |

### Suite 6: Unregistration & Lifecycle

*Objective: Verify that unregistration (handled via stream close) triggers correct cleanup on both client and server for all shutdown scenarios.*

Unregistration does not use explicit protocol messages. Closing the stream is the sole unregistration mechanism. The client proxy delivers a synthetic `on_unregister()` callback to the trait implementation via `on_close`. The server proxy removes the component from the BPA and cancels the proxy infrastructure.

| Test ID | Scenario | Setup | Expected Behavior | Status |
| ----- | ----- | ----- | ----- | ----- |
| **LIFE-01** | **Client-initiated unregister** | Client calls `Sink::unregister()`. | Client proxy shuts down, stream closes. Server `on_close` fires: takes sink, calls `sink.unregister()` (BPA removes component), cancels proxy. Client `on_close` delivers synthetic `trait.on_unregister()`. | Not implemented |
| **LIFE-02** | **BPA-initiated unregister** | BPA calls `shutdown_agents()` (or equivalent). | Server `on_unregister()` takes sink, calls `proxy.shutdown()`. Stream closes. Client `on_close` delivers synthetic `trait.on_unregister()`. | Not implemented |
| **LIFE-03** | **Drop without unregister** | Client drops proxy without calling `unregister()`. | Proxy `Drop` cancels tasks, stream closes. Server `on_close` fires: takes sink, calls `sink.unregister()`, cancels proxy. BPA removes component. | Not implemented |
| **LIFE-04** | **Server crash** | Server stream drops unexpectedly. | Client reader detects error/close, `on_close` fires: delivers synthetic `trait.on_unregister()` to trait impl. | Not implemented |
| **LIFE-05** | **Race: simultaneous unregister** | Client and BPA unregister concurrently. | `Mutex<Option>.take()` ensures exactly one path takes the sink. No double-unregister, no deadlock. | Not implemented |
| **LIFE-06** | **Synthetic on_unregister exactly once** | BPA sends `on_unregister` then stream closes. | Client trait impl receives `on_unregister()` exactly once (from `on_close`), not twice. | Not implemented |

### Suite 7: Server Proxy Handlers

*Objective: Verify server-side proxy implementations correctly manage sink ownership and proxy lifecycle.*

| Test ID | Scenario | Setup | Expected Behavior | Status |
| ----- | ----- | ----- | ----- | ----- |
| **SRV-01** | **Registration handshake** | Client sends `RegisterRequest` as first message. | Server creates `RemoteXxx`, registers with BPA via `BpaRegistration`, stores sink, starts proxy. | Not implemented |
| **SRV-02** | **Sink available after register** | Handler receives a request after registration. | `sink()` returns `Ok(Arc<dyn Sink>)`, operation forwarded to BPA. | Not implemented |
| **SRV-03** | **Sink unavailable after unregister** | Handler receives a request after sink has been taken. | `sink()` returns `Err(Unavailable)`, handler returns error response. | Not implemented |
| **SRV-04** | **Spin lock not held across await** | `on_close` calls `unregister()`, BPA callback re-enters `on_unregister()`. | No deadlock. Spin lock released before `sink.unregister().await`. | Not implemented |
| **SRV-05** | **on_close cancels proxy** | Stream closes after `on_close` completes. | `proxy.on_unregister()` called, writer task exits, tonic connection freed. | Not implemented |
| **SRV-06** | **on_unregister drains proxy (BPA-initiated)** | BPA calls `on_unregister()` with sink present. | `proxy.shutdown().await` called, handler and infrastructure tasks drain before returning. | Not implemented |

## 4. Execution Strategy

These tests are implemented as integration tests within the `hardy-proto` package.

* **Suites 1–4 (Client message mapping):** `tests/client_tests.rs` — mock server approach
* **Suite 5 (Error handling):** `tests/client_tests.rs` — mock server approach
* **Suites 6–7 (Lifecycle & Server):** `tests/lifecycle_tests.rs` — paired mock client/server
* **Command:** `cargo test -p hardy-proto`
* **Dependencies:** `tokio`, `tonic`, `proptest` (optional for fuzzing inputs).
