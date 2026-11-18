# Component Test Plan: gRPC Proxies

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | gRPC Proxy Implementation |
| **Module** | `hardy-proto` (Client & Server Proxies) |
| **Requirements Ref** | [REQ-18](../../docs/requirements.md#req-18-comprehensive-technical-documentation-and-examples) |
| **Test Suite ID** | COMP-GRPC-01 |
| **Version** | 1.1 |

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

### Suite 2: CLA Client Proxy

*Objective: Verify the `ClaClient` correctly maps Rust types to `cla.proto` messages.*

| Test ID | Scenario | Client Action (Rust) | Mock Server Assertion | Status |
| ----- | ----- | ----- | ----- | ----- |
| **CLA-CLI-01** | **Registration** | Call `register("tcp-1", cla::AddressType::Tcp)` | Receives `RegisterClaRequest { name: "tcp-1", address_type: TCP }`.<br>Replies `RegisterClaResponse`. | Implemented |
| **CLA-CLI-02** | **Dispatch Bundle** | Call `dispatch(bundle_bytes)` | Receives `DispatchBundleRequest { bundle: ... }`.<br>Replies `DispatchBundleResponse`. | Implemented |
| **CLA-CLI-03** | **Forward Bundle** | Await `next_message()` | Server injects `ForwardBundleRequest`.<br>Client yields `ClaMsg::ForwardBundle(...)`. | Implemented |
| **CLA-CLI-04** | **Add Peer** | Call `add_peer(node_id, address)` | Receives `AddPeerRequest`.<br>Replies `AddPeerResponse`. | Implemented |
| **CLA-CLI-05** | **Remove Peer** | Call `remove_peer(node_id)` | Receives `RemovePeerRequest`.<br>Replies `RemovePeerResponse`. | Implemented |

### Suite 3: Service Client Proxy

*Objective: Verify the `ServiceClient` correctly maps Rust types to `service.proto` messages (Service RPC).*

| Test ID | Scenario | Client Action (Rust) | Mock Server Assertion | Status |
| ----- | ----- | ----- | ----- | ----- |
| **SVC-CLI-01** | **Registration (IPN)** | Call `register_service(Some(Ipn(42)))` | Receives `RegisterRequest { service_id: { ipn: 42 } }`.<br>Replies `RegisterResponse { endpoint_id: "ipn:1.42" }`. | Implemented |
| **SVC-CLI-02** | **Send Raw Bundle** | Call `sink.send(bundle_bytes)` | Receives `ServiceSendRequest { data: ... }`.<br>Replies `SendResponse { bundle_id }`. | Implemented |
| **SVC-CLI-03** | **Receive Raw Bundle** | Server injects `ServiceReceiveRequest`. | Client trait receives `on_deliver(bundle_id, stream, expiry, total_len)`. | Implemented |
| **SVC-CLI-04** | **Status Notification** | Server injects `StatusNotifyRequest`. | Client trait receives `on_status_notify(...)`. | Implemented |

### Suite 4: Routing Agent Client Proxy

*Objective: Verify the Routing Agent client correctly maps Rust types to `routing.proto` messages.*

| Test ID | Scenario | Client Action (Rust) | Mock Server Assertion | Status |
| ----- | ----- | ----- | ----- | ----- |
| **RTE-CLI-01** | **Registration** | Call `register_routing_agent("tvr", agent)` | Receives `RegisterRoutingAgentRequest { name: "tvr" }`.<br>Replies `RegisterRoutingAgentResponse { node_ids }`. The agent is built from a context carrying the sink and the node IDs. | Implemented |
| **RTE-CLI-02** | **Add Route** | Call `sink.add_route(pattern, action, priority)` | Receives `AddRouteRequest { pattern, action, priority }`.<br>Replies `AddRouteResponse { added: true }`. | Implemented |
| **RTE-CLI-03** | **Remove Route** | Call `sink.remove_route(pattern, action, priority)` | Receives `RemoveRouteRequest { pattern, action, priority }`.<br>Replies `RemoveRouteResponse { removed: true }`. | Implemented |
| **RTE-CLI-04** | **Refused registration** | Register a second agent under a name the BPA has already given out | The registration fails `AlreadyExists`, carrying the reason the BPA gave, rather than ending as a dropped stream. | Implemented |

### Suite 5: Error Handling & Lifecycle

*Objective: Verify the client handles protocol violations and connection issues gracefully.*

*ERR-CLI-01 (Connection Refused) removed — it only exercised tonic's transport error path, not Hardy-specific logic.*

| Test ID | Scenario | Setup | Expected Behavior | Status |
| ----- | ----- | ----- | ----- | ----- |
| **ERR-CLI-02** | **Premature Stream End** | Server closes stream immediately after handshake. | Client `on_close` fires, delivering synthetic `on_unregister()` to trait impl. | Implemented |
| **ERR-CLI-03** | **Protocol Violation** | Server sends `RegisterRoutingAgentResponse` *twice*. | Client proxy ignores duplicate (no pending entry for msg_id 0), no panic. | Implemented |
| **ERR-CLI-04** | **Invalid Message Sequence** | Server sends `AddRouteResponse` with wrong msg\_id before registration response. | Client handshake returns `Err(Status::aborted("Out of sequence response"))`. | Implemented |

### Suite 6: Unregistration & Lifecycle

*Objective: Verify that unregistration (handled via stream close) triggers correct cleanup on both client and server for all shutdown scenarios.*

Unregistration does not use explicit protocol messages. Closing the stream is the sole unregistration mechanism. The client proxy shuts the session's component down via `on_close`. The server proxy removes the component from the BPA and cancels the proxy infrastructure.

*LIFE-03 (Drop without unregister) removed — the session owns the agent, the agent owns the sink, and the sink owns the proxy, so a client cannot drop the proxy without unregistering.*

| Test ID | Scenario | Setup | Expected Behavior | Status |
| ----- | ----- | ----- | ----- | ----- |
| **LIFE-01** | **Client-initiated unregister** | Client calls `Sink::unregister()`. | Client proxy shuts down, stream closes. Server `on_close` fires: unregisters the BPA-side sink and cancels the proxy. Client `on_close` shuts the session's agent down. | Implemented |
| **LIFE-02** | **BPA-initiated unregister** | BPA shuts the server-side agent down. | The agent's `shutdown()` shuts the proxy down, stream closes. Client `on_close` shuts the session's agent down. | Implemented |
| **LIFE-04** | **Server crash** | Server stream drops unexpectedly. | Client reader detects error/close, `on_close` fires and shuts the session's agent down. | Implemented |
| **LIFE-05** | **Race: simultaneous unregister** | Client and BPA unregister concurrently. | `Mutex<Option>::take()` ensures exactly one path takes the agent. No double shutdown, no deadlock. | Implemented |
| **LIFE-06** | **Shutdown exactly once** | BPA closes the stream after unregistering. | The client agent is shut down exactly once (from `on_close`), not twice. | Implemented |

### Suite 7: Server Proxy Handlers

*Objective: Verify server-side proxy implementations correctly translate wire requests onto the BPA sink they were built with.*

*SRV-01 (Registration handshake) removed — covered by every Suite 1–4 registration test.*
*SRV-05 (on\_close cancels proxy) removed — covered by LIFE-01 and LIFE-04.*
*SRV-06 (on\_unregister drains proxy) removed — covered by LIFE-02.*

| Test ID | Scenario | Setup | Expected Behavior | Status |
| ----- | ----- | ----- | ----- | ----- |
| **SRV-02** | **Added route reaches the sink** | An `AddRouteRequest` arrives for a registered agent. | The pattern, action and priority are translated and handed to the BPA sink unchanged. | Implemented |
| **SRV-03** | **Removed route reaches the sink** | A `RemoveRouteRequest` arrives for a registered agent. | The route is withdrawn from the BPA sink. | Implemented |
| **SRV-04** | **Malformed route request refused** | An `AddRouteRequest` carries an unparsable pattern. | The call fails `InvalidArgument` and the sink is left untouched. | Implemented |

## 4. Execution Strategy

These tests are implemented as integration tests within the `hardy-proto` package.

* **Suite 1 (Application client):** `tests/application_tests.rs` — mock server approach
* **Suite 2 (CLA client):** `tests/cla_tests.rs` — mock server approach
* **Suite 3 (Service client):** `tests/service_tests.rs` — mock server approach
* **Suite 4 (Routing agent client):** `tests/routing_agent_tests.rs` — mock server approach
* **Suite 5 (Error handling):** `src/client/routing.rs` unit tests (ERR-CLI-02/03/04)
* **Suite 6 (Lifecycle):** `tests/lifecycle_tests.rs` — paired mock client/server
* **Suite 7 (Server proxy handlers):** `src/server/routing.rs` unit tests (SRV-02/03/04)
* **Command:** `cargo test -p hardy-proto`
* **Dependencies:** `tokio`, `tonic`, `proptest` (optional for fuzzing inputs).
