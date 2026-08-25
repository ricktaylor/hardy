# Component Test Plan: gRPC API v1

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | gRPC API v1 (wire contract, server bridges, client SDK) |
| **Module** | `hardy-proto` |
| **Requirements Ref** | [REQ-18](../../docs/requirements.md#req-18-comprehensive-technical-documentation-and-examples) |
| **Test Suite ID** | COMP-GRPC-01 |
| **Version** | 2.0 |

## 1. Introduction

This document details the component testing strategy for the v1 gRPC surfaces of `hardy-proto`: the four server bridges (`ApplicationServiceImpl`, `ServiceServiceImpl`, `ClaServiceImpl`, `RoutingAgentServiceImpl`), the shared session machinery they ride on, and the `BpaClient` SDK, all defined in the [design document](design.md).

**Scope:**

- **Server bridges over the real wire:** each bridge is mounted on a real TCP listener and driven by its generated tonic client, against a real in-process `hardy_bpa::Bpa`.
- **Session lifecycle:** registration handshake, token minting and invalidation, and every in-crate teardown path (explicit `Unregister`, dropped rpc, pool shutdown).
- **Data plane:** the chunked-transfer grammar in both directions, including commit (`last_chunk`), truncation, in-band cancellation, and the parked-work semantics of abandoned deliveries and forwardings.
- **Client SDK end to end:** one roundtrip per surface through `BpaClient` against the same bridges, verifying that a component behind the SDK behaves as a local registration would.

**Out of scope:**

- BPA-internal logic (dispatch, RIB, storage, filters): verified by [`PLAN-BPA-01`](../../bpa/docs/component_test_plan.md) and its sibling plans.
- Network transport reliability (TCP/IP, HTTP/2 framing): tonic's concern.
- The cross-crate lifecycle scenarios enumerated in section 5, which need infrastructure this crate's in-process tests do not have.

## 2. Test Doctrine

The tests follow one doctrine, uniform across the suites:

1. **A real `Bpa`, never mocks of the wire.** Each surface's harness builds a real `hardy_bpa::Bpa` (node `ipn:1`, in-memory storage, built with the `no-rfc9171-autoregister` dev feature so the auto-registered ingress filter does not sit between the wire and the assertions), mounts the bridge on it, and shuts the BPA down at the end of every test. Nothing on either side of the wire is mocked: what is asserted is the observable contract, not calls into a fake.
2. **Port-0 listeners.** Every harness binds `127.0.0.1:0` and connects the generated client to the assigned port, so suites parallelise without port coordination.
3. **Event-driven waits.** Positive assertions ride the session's own events (the `Registration` handshake, `Delivery` and `Forwarding` announcements, stream endings), each wrapped in a 10 second guard timeout, never in fixed delays. Two bounded exceptions are deliberate: negative assertions ("a truncated send must not deliver") race the event stream against a short timeout, and token invalidation is asserted by polling for the `UNAUTHENTICATED` rejection, because teardown completes asynchronously after the stream closes and the rejection itself is the only client-visible signal.
4. **Every cancellation direction the crate can exercise by itself.** In-band cancel of an inbound transfer (SVC-04, CLA-06), abandonment of a collection (APP-05, SVC-06) and of a forwarding (CLA-03), truncation by half-close (APP-03, SVC-03, CLA-05), the client vanishing mid-session (APP-07, SVC-09), explicit `Unregister` on all four surfaces (APP-11, SVC-11, CLA-09, RTE-06), host pool shutdown (APP-08), and teardown racing a blocked event send (SES-04). Every abandonment test also asserts the deferral contract: the bundle stays parked, and the next announcement (the re-announced forwarding, or the next registration's delivery) succeeds.
5. **The send-to-self roundtrip is the smoke test of each payload surface** (APP-02, SVC-02, and the dispatch-and-forward loop CLA-02): it proves the announce-and-collect pipeline live end to end, byte-for-byte where the surface promises it (the service surface returns the stored bundle exactly; the CLA surface asserts the announced size and payload survival, because the BPA rewrites extension blocks at egress).

The tests live beside the code they pin, as `#[cfg(test)]` modules in `src/server/session.rs` and `src/server/services/{application,service,cla,routing}.rs`. The four client SDK roundtrips are additionally gated on the `client` feature, because they drive the bridges through `BpaClient` instead of the generated clients.

## 3. Test Suites

### Suite SES: shared session state (`server/session.rs`)

*Objective: pin the invariants of `Session`, `Sessions`, and `SessionStream` that every surface relies on, without a network.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **SES-01** | `sessions_resolve_only_live_tokens` | Mint, publish, one-probe resolve; a forged token and a retired token are `UNAUTHENTICATED`; removal is idempotent | Implemented |
| **SES-02** | `the_registration_precedes_events_then_the_stream_ends_on_abort` | The `Registration` is the stream's first item even against an earlier-accepted event; accepted events drain after abort; the stream then ends without any sender being dropped by hand | Implemented |
| **SES-03** | `abort_fires_the_broadcast_and_stops_events` | `abort` cancels the session token and the biased race refuses events even with buffer space free | Implemented |
| **SES-04** | `event_blocked_on_a_full_buffer_is_freed_by_teardown` | A send parked on a full event buffer is released by teardown alone, so a slow consumer cannot wedge a dying session | Implemented |

### Suite APP: application surface (`server/services/application.rs`)

*Objective: verify the `application.v1` wire against a real BPA, including the send accumulation scaffold and the parked-delivery semantics.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **APP-01** | `explicit_and_dynamic_registrations_mint_distinct_sessions` | Explicit ipn registration resolves to `ipn:1.7`; a dynamic registration gets a distinct endpoint and token | Implemented |
| **APP-02** | `send_to_self_roundtrip` | Send commits, the `Delivery` announces the same real bundle id with the right size and source, collection returns the ADU, and the completed collection consumes the delivery (`NOT_FOUND` after) | Implemented |
| **APP-03** | `a_truncated_send_never_commits` | Half-close without `last_chunk` is `ABORTED` and nothing is submitted (no delivery follows) | Implemented |
| **APP-04** | `receive_of_an_unannounced_id_is_not_found` | An id never announced to this session (malformed ids included) is `NOT_FOUND` at the `Receive` door | Implemented |
| **APP-05** | `an_abandoned_collection_defers_to_the_next_registration` | An in-band cancel mid-collection is `CANCELLED`; the spent announcement answers `NOT_FOUND`, and the next registration is re-announced the parked bundle and collects the whole ADU | Implemented |
| **APP-06** | `a_forged_token_is_rejected` | A forged token on a door is `UNAUTHENTICATED` | Implemented |
| **APP-07** | `a_dropped_stream_tears_the_session_down` | Dropping the rpc without `Unregister` fires the response-stream guard and invalidates the token | Implemented |
| **APP-08** | `pool_shutdown_tears_sessions_and_drains` | Host pool shutdown ends the session stream with no client action and `TaskPool::shutdown` drains | Implemented |
| **APP-09** | `client_sdk_roundtrip` | An application behind `BpaClient` registers, sends through its sink, and pulls its own delivery to completion | Implemented (`client` feature) |
| **APP-10** | `re_registration_re_announces_many_parked_deliveries` | 48 uncollected deliveries survive an unregister; re-registration completes promptly and every parked bundle is re-announced and collectable | Implemented |
| **APP-11** | `unregister_ends_the_session_and_invalidates_the_token` | The wire's `Unregister` ends the stream and the token dies in the session task's teardown | Implemented |

### Suite SVC: service surface (`server/services/service.rs`)

*Objective: verify the `service.v1` wire, including the native streamed `send` path and the BPA's validation at the trust boundary.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **SVC-01** | `explicit_and_dynamic_registrations_mint_distinct_sessions` | As APP-01, for the service surface | Implemented |
| **SVC-02** | `send_to_self_roundtrip` | A canonical bundle round-trips byte-identical through send, delivery, and collection; the completed collection consumes the delivery | Implemented |
| **SVC-03** | `a_truncated_send_never_commits` | Truncation through the streamed pump is `ABORTED` and nothing is submitted | Implemented |
| **SVC-04** | `a_cancelled_send_is_discarded` | The wire's in-band `cancel` ends the call `CANCELLED` and the partial bundle is discarded | Implemented |
| **SVC-05** | `an_invalid_bundle_is_rejected` | Raw garbage fails BPA validation with `INVALID_ARGUMENT`; nothing enters the store | Implemented |
| **SVC-06** | `an_abandoned_collection_defers_to_the_next_registration` | As APP-05, for whole bundles | Implemented |
| **SVC-07** | `a_forged_token_is_rejected` | As APP-06 | Implemented |
| **SVC-08** | `a_forged_source_is_rejected` | A bundle claiming another endpoint as its source is rejected at the trust boundary with `INVALID_ARGUMENT` | Implemented |
| **SVC-09** | `a_dropped_stream_tears_the_session_down` | As APP-07 | Implemented |
| **SVC-10** | `client_sdk_roundtrip` | A service behind `BpaClient` sends a whole bundle through the streamed pump and collects its own delivery | Implemented (`client` feature) |
| **SVC-11** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-11 | Implemented |

### Suite CLA: convergence-layer surface (`server/services/cla.rs`)

*Objective: verify the `cla.v1` wire, including the `Forward` rendezvous, the peer doors, and the deferred-outcome path.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **CLA-01** | `registration_returns_node_ids_and_a_token` | Registration returns the BPA's node ids; a duplicate CLA name fails the `Subscribe` call with `ALREADY_EXISTS` | Implemented |
| **CLA-02** | `dispatch_and_forward_roundtrip` | A dispatched bundle for a destination behind an announced peer is routed back out: the `Forwarding` carries the peer address, the streamed bundle matches the announced size and carries the payload, and the `sent` result completes the call cleanly | Implemented |
| **CLA-03** | `an_abandoned_forwarding_stays_queued` | An in-band cancel mid-`Forward` is `CANCELLED`, the BPA requeues, and the re-announced forwarding streams the whole bundle and completes | Implemented |
| **CLA-04** | `an_accepted_forwarding_reports_its_outcome` | An `accepted` result parks the transfer for the unary `ReportTransferOutcome`; a second outcome for a finished transfer is dropped, not an error | Implemented |
| **CLA-05** | `a_truncated_dispatch_never_commits` | Truncated dispatch is `ABORTED` and no forwarding is announced | Implemented |
| **CLA-06** | `a_cancelled_dispatch_is_discarded` | The wire's in-band `cancel` ends the dispatch `CANCELLED` | Implemented |
| **CLA-07** | `peers_are_added_and_removed_once` | `AddPeer`/`RemovePeer` idempotence: `added`/`removed` are true once, then false | Implemented |
| **CLA-08** | `a_forged_token_is_rejected` | As APP-06, on a unary door | Implemented |
| **CLA-09** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-11 | Implemented |
| **CLA-10** | `a_forward_for_an_unknown_bundle_is_not_found` | A `Forward` presenting an unannounced bundle id is `NOT_FOUND` (no parked rendezvous) | Implemented |
| **CLA-11** | `forward_requires_the_metadata_first` | A `Forward` whose first message is not the metadata is `INVALID_ARGUMENT` | Implemented |
| **CLA-12** | `client_sdk_roundtrip` | A CLA behind `BpaClient` announces a peer, dispatches, and receives the bundle back through `Cla::forward` via the SDK's forwarding executor | Implemented (`client` feature) |

### Suite RTE: routing surface (`server/services/routing.rs`)

*Objective: verify the `routing.v1` wire: the push-only session and the two unary route doors.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **RTE-01** | `registration_returns_node_ids_and_a_token` | Registration returns the BPA's node ids; a duplicate agent name fails the `Subscribe` call with `ALREADY_EXISTS` | Implemented |
| **RTE-02** | `routes_are_added_and_removed_once` | `AddRoute`/`RemoveRoute` idempotence against the real RIB | Implemented |
| **RTE-03** | `an_invalid_pattern_is_rejected` | A malformed EID pattern is `INVALID_ARGUMENT` | Implemented |
| **RTE-04** | `a_missing_action_is_rejected` | A route with no action is `INVALID_ARGUMENT` | Implemented |
| **RTE-05** | `a_forged_token_is_rejected` | As APP-06 | Implemented |
| **RTE-06** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-11 | Implemented |
| **RTE-07** | `client_sdk_roundtrip` | A routing agent behind `BpaClient` drives add/remove idempotence through its sink | Implemented (`client` feature) |

Teardown mechanisms shared by every surface (the response-stream guard, the pool-shutdown broadcast) are pinned once where they are cheapest to observe (APP-07/SVC-09 and APP-08) rather than repeated on all four surfaces; the CLA-specific residue of a dropped stream (in-flight forwardings resolving as disconnection) is part of the deferred lifecycle suite below.

## 4. Execution Strategy

The tests are `#[cfg(test)]` modules inside the crate, compiled only with the `server` feature (and, for the SDK roundtrips, the `client` feature):

- `cargo test -p hardy-proto --features server` runs the 41 wire and session tests.
- `cargo test -p hardy-proto --all-features` runs all 45, adding the four `client_sdk_roundtrip` tests.
- CI runs the workspace with `--all-features`, so all 45 run on every change.
- Building the crate requires `protoc` (the schemas compile in `build.rs`).

All wire tests use the multi-threaded tokio runtime (`worker_threads = 2`), because a single-threaded runtime would serialise the server, the client, and the BPA's dispatcher tasks against each other.

## 5. The cross-crate lifecycle suite

The lifecycle scenarios live in `proto/tests/lifecycle.rs`: six tests driving the `BpaClient` SDK against a served bridge through the crate's public surface only. They cover client-initiated unregister round-tripping and freeing the service id, BPA-initiated teardown reaching the client, drop-without-unregister as a disconnection, a server restart (bridge teardown) disconnecting the client with its orphaned sink failing, connection loss deferring an announced-but-uncollected bundle to the endpoint's next registration, and simultaneous unregistration settling with exactly one observed `on_unregister`.

Still deferred, with the reasons unchanged:

| Scenario | Why it is deferred |
| :--- | :--- |
| **Silent transport death** | Distinguishing silent peer death, keepalive detection timing, and half-open connections from graceful stream closures needs a killable transport (a proxy or a hard-killed process); the in-process suite can only sever sessions cooperatively |
| **CLA vanished-client mid-`Forward` residue** | Abandonment and dropped-session teardown are pinned in-crate; a client vanishing mid-`Forward` drive (the rendezvous claimed, chunks in flight) should be pinned end to end once a killable transport exists |
