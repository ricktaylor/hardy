# Component Test Plan: gRPC API v1

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | gRPC API v1 (wire contract, server bridges, client SDK) |
| **Module** | `hardy-proto` |
| **Requirements Ref** | [REQ-18](../../docs/requirements.md#req-18-comprehensive-technical-documentation-and-examples) |
| **Test Suite ID** | COMP-GRPC-01 |
| **Version** | 3.0 |

## 1. Introduction

This document details the component testing strategy for the v1 gRPC surfaces of `hardy-proto`: the four server bridges (`ApplicationServiceImpl`, `ServiceServiceImpl`, `ClaServiceImpl`, `RoutingAgentServiceImpl`), the shared session and hold-table machinery they ride on, and the `BpaClient` SDK, all defined in the [design document](design.md).

**Scope:**

- **Server bridges over the real wire:** each bridge is mounted on a real TCP listener and driven by its generated tonic client, against a real in-process `hardy_bpa::Bpa`.
- **Session lifecycle:** registration handshake, token minting and invalidation, and every in-crate teardown path (explicit `Unregister`, dropped rpc, pool shutdown).
- **Data plane:** the chunked-transfer grammar in both directions, including commit (`last_chunk`), truncation, in-band cancellation, the ack-gated delivery commit protocol, and the parked-work semantics of abandoned deliveries and forwardings.
- **Client SDK end to end:** roundtrips and the SDK-specific commit paths per surface through `BpaClient` against the same bridges, verifying that a component behind the SDK behaves as a local registration would.

**Out of scope:**

- BPA-internal logic (dispatch, RIB, storage, filters): verified by [`PLAN-BPA-01`](../../bpa/docs/component_test_plan.md) and its sibling plans.
- Network transport reliability (TCP/IP, HTTP/2 framing): tonic's concern.
- The killable-transport lifecycle scenarios enumerated in section 5, which need infrastructure this crate's in-process tests do not have.

## 2. Test Doctrine

The tests follow one doctrine, uniform across the suites:

1. **A real `Bpa`, never mocks of the wire.** Each surface's harness builds a real `hardy_bpa::Bpa` (node `ipn:1`, in-memory storage, built with the `no-rfc9171-autoregister` dev feature so the auto-registered ingress filter does not sit between the wire and the assertions), mounts the bridge on it, and shuts the BPA down at the end of every test. Nothing on either side of the wire is mocked: what is asserted is the observable contract, not calls into a fake.
2. **Port-0 listeners.** Every harness binds `127.0.0.1:0` and connects the generated client to the assigned port, so suites parallelise without port coordination.
3. **Event-driven waits.** Positive assertions ride the session's own events (the `Registration` handshake, `Delivery` and `Forwarding` announcements, stream endings), each wrapped in a 10 second guard timeout that only bounds a regression, never in fixed delays. Two bounded exceptions are deliberate: negative assertions ("a truncated send must not deliver") race the event stream against a short timeout, and token invalidation synchronizes on the `torn_down()` broadcast barrier the session machinery raises in teardown (`wait_torn_down`) before asserting the `UNAUTHENTICATED` rejection, so the assertion is race-free rather than polled.
4. **Every cancellation direction the crate can exercise by itself.** In-band cancel of an inbound transfer (SVC-04, CLA-06) and of the application `Send` accumulation loop (APP-04), abandonment of a collection (APP-06, SVC-06) and of a forwarding (CLA-03), truncation by half-close (APP-03, SVC-03, CLA-05), the client vanishing mid-session on all four surfaces (APP-08, SVC-09, CLA-09, RTE-06), explicit `Unregister` on all four surfaces (APP-25, SVC-12, CLA-10, RTE-07), host pool shutdown (APP-09, and APP-17 with a claimed unread collection alive), and teardown racing a blocked event send (SES-04). Every abandonment test also asserts the deferral contract: the bundle stays parked, and the next announcement (the re-announced forwarding, or the next registration's delivery) succeeds.
5. **The delivery commit is ack-gated, and pinned in every arm** (APP-14 through APP-16, APP-13, and the SDK's APP-22): completion is the client's ack, never the last chunk. A cancel after the final chunk, a full receipt followed by silence, an ack racing ahead of the final chunk (a protocol violation), and a session death mid-collection all leave the bundle parked and re-announced; only a conforming ack commits.
6. **The send-to-self roundtrip is the smoke test of each payload surface** (APP-02, SVC-02, and the dispatch-and-forward loop CLA-02): it proves the announce-and-collect pipeline live end to end, byte-for-byte where the surface promises it (the service surface returns the stored bundle exactly; the CLA surface asserts the announced size and payload survival, because the BPA rewrites extension blocks at egress).

The tests live beside the code they pin, as `#[cfg(test)]` modules in `src/server/session.rs`, `src/server/services/mod.rs` (the shared hold table), and `src/server/services/{application,service,cla,routing}.rs`, with the shared harness and fixtures in an inline `#[cfg(test)] pub mod tests` in `mod.rs` cross-imported by path. The ten SDK-driven in-crate tests and the nine cross-crate lifecycle tests are additionally gated on the `client` feature, because they drive the bridges through `BpaClient` instead of the generated clients.

## 3. Test Suites

### Suite SES: shared session state (`server/session.rs`)

*Objective: pin the invariants of `Session`, `Sessions`, and `SessionStream` that every surface relies on, without a network.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **SES-01** | `sessions_resolve_only_live_tokens` | Mint, publish, one-probe resolve; a forged token and a retired token are `UNAUTHENTICATED`; removal is idempotent | Implemented |
| **SES-02** | `the_registration_precedes_events_then_the_stream_ends_on_abort` | The `Registration` is the stream's first item even against an earlier-accepted event; accepted events drain after abort; the stream then ends without any sender being dropped by hand | Implemented |
| **SES-03** | `abort_fires_the_broadcast_and_stops_events` | `abort` cancels the session token and the biased race refuses events even with buffer space free | Implemented |
| **SES-04** | `event_blocked_on_a_full_buffer_is_freed_by_teardown` | A send parked on a full event buffer is released by teardown alone, so a slow consumer cannot wedge a dying session | Implemented |

### Suite REG: shared claim/hold table (`server/services/mod.rs`)

*Objective: pin the hold-table primitive that the delivery and forward rendezvous share, without a network.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **REG-01** | `a_claim_is_single_use_and_withdraw_removes` | A held entry's claim is single-use: a second claim of the same id misses, and `withdraw` removes the entry | Implemented |
| **REG-02** | `dead_entries_are_swept_once_the_threshold_is_reached` | Dead entries accumulate without leaking: the table sweeps them once the accumulation threshold is reached | Implemented |

### Suite APP: application surface (`server/services/application.rs`)

*Objective: verify the `hardy.application.v1` wire against a real BPA, including the send accumulation scaffold, the ack-gated commit, and the parked-delivery semantics.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **APP-01** | `explicit_and_dynamic_registrations_mint_distinct_sessions` | Explicit ipn registration resolves to `ipn:1.7`; a dynamic registration gets a distinct endpoint and token | Implemented |
| **APP-02** | `send_to_self_roundtrip` | Send commits, the `Delivery` announces the same real bundle id with the right size and source, collection returns the ADU, and the completed collection consumes the delivery (`NOT_FOUND` after) | Implemented |
| **APP-03** | `a_truncated_send_never_commits` | Half-close without `last_chunk` is `ABORTED` and nothing is submitted (no delivery follows) | Implemented |
| **APP-04** | `a_cancelled_send_is_discarded` | The `Send` accumulation loop's in-band `cancel` ends the call `CANCELLED` and the partial ADU is discarded | Implemented |
| **APP-05** | `receive_of_an_unannounced_id_is_not_found` | An id never announced to this session (malformed ids included) is `NOT_FOUND` at the `Receive` door | Implemented |
| **APP-06** | `an_abandoned_collection_defers_to_the_next_registration` | An in-band cancel mid-collection is `CANCELLED`; the spent announcement answers `NOT_FOUND`, and the next registration is re-announced the parked bundle and collects the whole ADU | Implemented |
| **APP-07** | `a_forged_token_is_rejected` | A forged token on a door is `UNAUTHENTICATED` | Implemented |
| **APP-08** | `a_dropped_stream_tears_the_session_down` | Dropping the rpc without `Unregister` fires the response-stream guard and invalidates the token | Implemented |
| **APP-09** | `pool_shutdown_tears_sessions_and_drains` | Host pool shutdown ends the session stream with no client action and `TaskPool::shutdown` drains | Implemented |
| **APP-10** | `an_empty_adu_delivers_end_to_end` | An empty ADU delivers as a lone empty `last_chunk` completion, never a truncation | Implemented |
| **APP-11** | `a_declared_adu_size_above_the_bound_is_rejected_preflight` | An above-bound declared size is rejected before any bytes arrive; within the bound the declaration is only a hint, and an inaccurate one still commits on `last_chunk` | Implemented |
| **APP-12** | `a_receive_racing_the_announcement_lands` | The stream is held before the `Delivery` event goes out, so a `Receive` racing the announcement lands once the entry exists; an early `NOT_FOUND` neither consumes nor poisons | Implemented |
| **APP-13** | `session_death_mid_receive_defers_the_delivery` | A session dying with a claimed `Receive` mid-stream and the final segment unpulled leaves the bundle parked; the next registration is announced it afresh and collects whole | Implemented |
| **APP-14** | `a_cancel_after_the_last_chunk_parks_the_delivery` | Completion is the ack, not the last chunk: a cancel after the final chunk with no ack abandons the collection and the bundle is re-announced | Implemented |
| **APP-15** | `a_full_receipt_without_an_ack_parks_the_delivery` | A client that takes the whole ADU and then goes silent commits nothing; the bundle is re-announced to the next registration | Implemented |
| **APP-16** | `an_ack_before_the_final_chunk_never_commits` | An ack racing the drain is a protocol violation, never a commit; the collection ends without its last chunk and the bundle is re-announced | Implemented |
| **APP-17** | `pool_shutdown_survives_a_claimed_unread_receive` | A client that claims a large collection and stops reading, keeping its connection alive, does not wedge pool shutdown: the parked pump abandons its terminal status | Implemented |
| **APP-18** | `a_stalled_session_does_not_starve_other_registrations` | A stalled session parks its announcements on its own tasks past its event buffer while other registrations keep delivering | Implemented |
| **APP-19** | `a_dtn_registration_needs_a_dtn_node_id` | A `dtn`-scheme registration on an ipn-only node fails the handshake with `FAILED_PRECONDITION` | Implemented |
| **APP-20** | `a_dtn_registration_binds_the_dtn_endpoint` | A `dtn`-scheme registration binds the `dtn` endpoint on a node that declares a `dtn` node id | Implemented |
| **APP-21** | `client_sdk_roundtrip` | An application behind `BpaClient` registers, sends through its sink, and pulls its own delivery to completion | Implemented (`client` feature) |
| **APP-22** | `an_sdk_decline_after_full_receipt_is_redelivered` | An SDK app returning `Err` from `on_deliver` after buffering the whole ADU sends no ack; the bundle stays parked and the endpoint's next registration receives it | Implemented (`client` feature) |
| **APP-23** | `a_delivery_report_reaches_the_sending_application` | A requested delivery report round-trips: the collected delivery generates it, the BPA consumes it at its admin endpoint, and the sending application is notified through the wire | Implemented (`client` feature) |
| **APP-24** | `re_registration_re_announces_many_parked_deliveries` | 48 uncollected deliveries survive an unregister; re-registration completes promptly and every parked bundle is re-announced and collectable | Implemented |
| **APP-25** | `unregister_ends_the_session_and_invalidates_the_token` | The wire's `Unregister` ends the stream and the token dies in the session task's teardown | Implemented |
| **APP-26** | `an_sdk_reply_from_within_a_delivery_does_not_deadlock` | An SDK app that replies through its own sink from inside `on_deliver` does not self-deadlock, even with more echoes in flight than the concurrent-delivery bound (the echo-over-gRPC shape) | Implemented (`client` feature) |

### Suite SVC: service surface (`server/services/service.rs`)

*Objective: verify the `hardy.service.v1` wire, including the native streamed `send` path and the BPA's validation at the trust boundary.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **SVC-01** | `explicit_and_dynamic_registrations_mint_distinct_sessions` | As APP-01, for the service surface | Implemented |
| **SVC-02** | `send_to_self_roundtrip` | A canonical bundle round-trips byte-identical through send, delivery, and collection; the completed collection consumes the delivery | Implemented |
| **SVC-03** | `a_truncated_send_never_commits` | Truncation through the streamed pump is `ABORTED` and nothing is submitted | Implemented |
| **SVC-04** | `a_cancelled_send_is_discarded` | The wire's in-band `cancel` ends the call `CANCELLED` and the partial bundle is discarded | Implemented |
| **SVC-05** | `an_invalid_bundle_is_rejected` | Raw garbage fails BPA validation with `INVALID_ARGUMENT`; nothing enters the store | Implemented |
| **SVC-06** | `an_abandoned_collection_defers_to_the_next_registration` | As APP-06, for whole bundles | Implemented |
| **SVC-07** | `a_forged_token_is_rejected` | As APP-07 | Implemented |
| **SVC-08** | `a_forged_source_is_rejected` | A bundle claiming another endpoint as its source is rejected at the trust boundary with `INVALID_ARGUMENT` | Implemented |
| **SVC-09** | `a_dropped_stream_tears_the_session_down` | As APP-08 | Implemented |
| **SVC-10** | `client_sdk_roundtrip` | A service behind `BpaClient` sends a whole bundle through the streamed pump and collects its own delivery | Implemented (`client` feature) |
| **SVC-11** | `a_delivery_report_reaches_the_sending_service` | A service-built bundle requesting a delivery report gets it back through the wire: the BPA consumes the report at its admin endpoint and the origin registration is notified | Implemented (`client` feature) |
| **SVC-12** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-25 | Implemented |

### Suite CLA: convergence-layer surface (`server/services/cla.rs`)

*Objective: verify the `hardy.cla.v1` wire, including the `Forward` rendezvous, the single-executor claim, the peer doors, the lane-count validation, and the deferred-outcome path.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **CLA-01** | `registration_returns_node_ids_and_a_token` | Registration returns the BPA's node ids; a duplicate CLA name fails the `Subscribe` call with `ALREADY_EXISTS` | Implemented |
| **CLA-02** | `dispatch_and_forward_roundtrip` | A dispatched bundle for a destination behind an announced peer is routed back out: the `Forwarding` carries the peer address, the streamed bundle matches the announced size and carries the payload, and the `sent` result completes the call cleanly | Implemented |
| **CLA-03** | `an_abandoned_forwarding_stays_queued` | An in-band cancel mid-`Forward` is `CANCELLED`, the BPA requeues, and the re-announced forwarding streams the whole bundle and completes | Implemented |
| **CLA-04** | `an_accepted_forwarding_reports_its_outcome` | An `accepted` result parks the transfer for the unary `ReportTransferOutcome`; a second outcome for a finished transfer is dropped, not an error | Implemented |
| **CLA-05** | `a_truncated_dispatch_never_commits` | Truncated dispatch is `ABORTED` and no forwarding is announced | Implemented |
| **CLA-06** | `a_cancelled_dispatch_is_discarded` | The wire's in-band `cancel` ends the dispatch `CANCELLED` | Implemented |
| **CLA-07** | `peers_are_added_and_removed_once` | `AddPeer`/`RemovePeer` idempotence: `added`/`removed` are true once, then false | Implemented |
| **CLA-08** | `a_forged_token_is_rejected` | As APP-07, on a unary door | Implemented |
| **CLA-09** | `a_dropped_stream_tears_the_session_down` | Dropping the rpc without `Unregister` fires the response-stream guard on the CLA surface and invalidates the token | Implemented |
| **CLA-10** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-25 | Implemented |
| **CLA-11** | `a_forward_for_an_unknown_bundle_is_not_found` | A `Forward` presenting an unannounced bundle id is `NOT_FOUND` (no parked rendezvous) | Implemented |
| **CLA-12** | `forward_requires_the_metadata_first` | A `Forward` whose first message is not the metadata is `INVALID_ARGUMENT` | Implemented |
| **CLA-13** | `a_duplicate_forward_for_a_live_call_is_not_found` | The `Forward` claim is single-executor: a duplicate `Forward` for an id a live call already holds answers `NOT_FOUND`, and the live call completes untouched | Implemented |
| **CLA-14** | `lane_count_is_validated_at_registration` | Zero and above-bound declared lane counts are `INVALID_ARGUMENT` at the wire boundary; the bound itself registers | Implemented |
| **CLA-15** | `an_sdk_deferred_outcome_completes_the_transfer` | A CLA behind `BpaClient` returning a deferred forwarding outcome later completes the transfer through `ReportTransferOutcome` | Implemented (`client` feature) |
| **CLA-16** | `the_sdk_rejects_an_over_declared_lane_count` | The SDK surfaces the registration-time rejection of an over-declared lane count | Implemented (`client` feature) |
| **CLA-17** | `client_sdk_roundtrip` | A CLA behind `BpaClient` announces a peer, dispatches, and receives the bundle back through `Cla::forward` via the SDK's forwarding executor | Implemented (`client` feature) |

### Suite RTE: routing surface (`server/services/routing.rs`)

*Objective: verify the `hardy.routing.v1` wire: the push-only session and the two unary route doors.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **RTE-01** | `registration_returns_node_ids_and_a_token` | Registration returns the BPA's node ids; a duplicate agent name fails the `Subscribe` call with `ALREADY_EXISTS` | Implemented |
| **RTE-02** | `routes_are_added_and_removed_once` | `AddRoute`/`RemoveRoute` idempotence against the real RIB | Implemented |
| **RTE-03** | `an_invalid_pattern_is_rejected` | A malformed EID pattern is `INVALID_ARGUMENT` | Implemented |
| **RTE-04** | `a_missing_action_is_rejected` | A route with no action is `INVALID_ARGUMENT` | Implemented |
| **RTE-05** | `a_forged_token_is_rejected` | As APP-07 | Implemented |
| **RTE-06** | `a_dropped_stream_tears_the_session_down` | Dropping the rpc without `Unregister` fires the response-stream guard on the routing surface and invalidates the token | Implemented |
| **RTE-07** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-25 | Implemented |
| **RTE-08** | `client_sdk_roundtrip` | A routing agent behind `BpaClient` drives add/remove idempotence through its sink, and the sink refuses the reserved drop reason before it reaches the wire | Implemented (`client` feature) |
| **RTE-09** | `a_reserved_drop_reason_is_rejected` | RFC 9171's reserved reason code 255 in a `drop` action is `INVALID_ARGUMENT`, while an unassigned code is carried through (the inbound half of the reserved-code contract; the SDK's outbound refusal rides RTE-08) | Implemented |

The response-stream guard is now pinned on all four surfaces (APP-08, SVC-09, CLA-09, RTE-06); the pool-shutdown broadcast is pinned where it is cheapest to observe (APP-09, APP-17). The CLA-specific residue of a *silently vanished* client mid-`Forward` (rendezvous claimed, chunks in flight) remains part of the deferred lifecycle suite below, because it needs a killable transport.

## 4. Execution Strategy

The in-crate tests are `#[cfg(test)]` modules inside the crate; the lifecycle tests are an integration test in `proto/tests/`. Both compile only with the `server` feature (and, for the SDK-driven tests, the `client` feature):

- `cargo test -p hardy-proto --features server` runs the 60 in-crate wire and unit tests that do not need the SDK.
- `cargo test -p hardy-proto --all-features` runs all 79, adding the ten SDK-driven in-crate tests and the nine cross-crate lifecycle tests.
- CI runs the workspace with `--all-features`, so all 79 run on every change.
- Building the crate requires `protoc` (the schemas compile in `build.rs`).

All wire tests use the multi-threaded tokio runtime (`worker_threads = 2`), because a single-threaded runtime would serialise the server, the client, and the BPA's dispatcher tasks against each other.

## 5. The cross-crate lifecycle suite

The lifecycle scenarios live in `proto/tests/lifecycle.rs`: nine tests driving the `BpaClient` SDK against a served bridge through the crate's public surface only.

| Test ID | Test function | What it pins |
| :--- | :--- | :--- |
| **LIF-01** | `a_client_unregister_round_trips` | A client `Unregister` ends the session, the SDK surfaces `on_unregister`, the registration handle resolves `Ok`, and the service id frees for a successor registration |
| **LIF-02** | `bpa_initiated_teardown_reaches_the_client` | Shutting the BPA down unregisters the bridge's component, ends the wire session, and the SDK surfaces `on_unregister` |
| **LIF-03** | `connection_loss_defers_announced_bundles` | A dead client's parked, uncollected bundle is re-announced to the endpoint's next registration, which collects it whole |
| **LIF-04** | `simultaneous_unregister_settles` | Unregistration from both ends settles with neither side hanging and exactly one observed `on_unregister` |
| **LIF-05** | `dropping_the_sink_unregisters` | An application that never stores its sink has disconnected by definition: the dropped sink half-closes the session, the server unregisters it, and the SDK surfaces `on_unregister` |
| **LIF-06** | `a_server_restart_disconnects_the_client` | A bridge teardown (a restart from the client's view) surfaces `on_unregister` and reads as a clean close on the handle; the orphaned sink fails rather than blocking, its token dead |
| **LIF-07** | `shutdown_interrupts_a_stuck_delivery` | An `on_deliver` that never returns is abandoned on client pool shutdown, and the session still runs its unregistration to completion |
| **LIF-08** | `deliveries_collect_concurrently` | Two announced bundles are inside `on_deliver` at once, so a slow collection does not stall the next announcement |
| **LIF-09** | `a_transport_loss_surfaces_the_session_error` | A connection killed without trailers is a stream failure, not a close: the registration handle yields the transport's own error carried whole (code and source chain intact), and the SDK still surfaces `on_unregister` |

Still deferred, with the reasons unchanged:

| Scenario | Why it is deferred |
| :--- | :--- |
| **Silent transport death** | Distinguishing silent peer death, keepalive detection timing, and half-open connections from graceful stream closures needs a transport that can go quiet without closing (a proxy or a hard-killed process); the in-process suite can kill connections outright (LIF-09) but cannot leave them silently half-open, which is what keepalive detection needs |
| **CLA vanished-client mid-`Forward` residue** | Abandonment and dropped-session teardown are pinned in-crate (CLA-03, CLA-09); a client vanishing mid-`Forward` drive (the rendezvous claimed, chunks in flight) should be pinned end to end once a killable transport exists |
