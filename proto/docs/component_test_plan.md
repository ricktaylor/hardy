# Component Test Plan: gRPC API v1

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | gRPC API v1 (wire contract, server APIs, client SDK) |
| **Module** | `hardy-proto` |
| **Requirements Ref** | [REQ-18](../../docs/requirements.md#req-18-comprehensive-technical-documentation-and-examples) |
| **Test Suite ID** | COMP-GRPC-01 |
| **Version** | 6.1 |

## 1. Introduction

This document details the testing strategy for the v1 gRPC APIs of `hardy-proto`: the four servers (`ApplicationServiceImpl`, `ServiceServiceImpl`, `ClaServiceImpl`, `RoutingServiceImpl`), the session machinery they share, and the `BpaClient` SDK, all defined in the [design document](design.md). The behaviour under test is specified in [`protocols/`](protocols/), one normative state machine per call family; the design document's [glossary](design.md#glossary) supplies the vocabulary. In particular a **stage** is the named point at which a client can stall; `server::Watchdog` bounds the session's own waits and records every stall, while a transfer is bounded by the `chunking::receiver::BoundedChunkReceiver` or `chunking::sender::BoundedChunkSender` that carries it, whose call records the stall on the watchdog through `Watchdog::stall`.

**Scope:**

- **Servers over the real wire:** each API is mounted on a real TCP listener and driven by its generated tonic client, against a real in-process `hardy_bpa::Bpa`.
- **Session lifecycle:** the registration handshake, the negotiated sizes, token minting and invalidation, and every in-crate teardown path (explicit `Unregister`, dropped rpc, pool shutdown on either side, a stalled stage).
- **Data plane:** the chunked-transfer grammar in both directions, including commit (`last_chunk`), truncation, in-band cancellation and withdrawal, the declared transfer size, the negotiated chunk size as a bound on both directions, the ack-gated delivery commit protocol, the deferred forwarding outcome, and the parked-work semantics of abandoned deliveries and forwardings.
- **Stall defences:** the bounded receiver's idle rule and rate floor and the watchdog's first-stall record over a paused clock, each stage reached over the wire with the matching limit set to zero, the handshake bound on every call that opens with a message, and the per-API session ceiling.
- **The opening of every call:** the message a call must open with, the fields it must carry, and the status a call that opens wrongly ends with.
- **Client SDK end to end:** roundtrips and the SDK-specific commit paths per API through `BpaClient` against the same servers, verifying that a component behind the SDK behaves as a local registration would.

**Out of scope:**

- BPA-internal logic (dispatch, RIB, storage, filters): verified by [`PLAN-BPA-01`](../../bpa/docs/component_test_plan.md) and its sibling plans.
- Network transport reliability (TCP/IP, HTTP/2 framing) and the transport-layer knobs a host configures: tonic's concern, recorded in [`TODO.md`](TODO.md).
- The killable-transport scenarios in section 5, which need infrastructure the in-process suite does not have.

## 2. Test Doctrine

The tests follow one doctrine, uniform across the suites:

1. **A real `Bpa`, never mocks of the wire.** Each harness builds a real `hardy_bpa::Bpa` (node `ipn:1`, in-memory storage, built with the `no-rfc9171-autoregister` dev feature so the auto-registered ingress filter does not sit between the wire and the assertions), mounts the server on it, and shuts the BPA down at the end of every test. Nothing on either side of the wire is mocked: what is asserted is the observable contract, not calls into a fake. The fakes that exist are local and minimal: `ParkedBpa` in the application suite, a `BpaRegistration` that parks inside `register_application` so a test can abandon an rpc inside the commit window; `SinkOnly` in the same suite, an in-process application that only keeps its sink, so a test can originate a bundle without going through the API whose bound it is testing; `StalledProducer` in the lifecycle suite, a segment source that never yields; and `killable_proxy` there too, a TCP proxy a test can kill.
2. **Port-0 listeners.** Every harness binds `127.0.0.1:0` and connects the generated client to the assigned port, so the suites parallelise without port coordination.
3. **Event-driven waits.** Positive assertions ride the session's own events (the `Registration` handshake, `Delivery` and `Forwarding` announcements, stream endings), each wrapped in a 10 second guard that only bounds a regression, never in fixed delays. Re-registration synchronizes on `UnregisterWatch`, a `BpaRegistration` decorator in `tests/common/mod.rs` that broadcasts when the BPA has actually run `on_unregister`, so a successor never races its predecessor's release. Negative assertions drive a real barrier: `drain_undelivered` and `assert_never_forwards` read the stream to its `UNAVAILABLE` ending and assert no announcement arrived on the way.
4. **Time is controlled, never waited on.** The watchdog's record and the bounded receiver's rate rules are pinned over a paused clock (`start_paused`): each test races the stall against a `sleep` inside a `select!`, so the paused clock advances to whichever fires first and a 30 second or 3600 second span costs nothing. Those two suites are the only place such a span appears. Every wire test that has to reach a stage sets the matching limit to `Duration::ZERO`, so the stall is immediate and deterministic rather than timed; the two handshake tests do the same with `handshake`.
5. **Every cancellation direction the crate can exercise by itself.** In-band cancel of an inbound transfer (`a_cancelled_send_is_discarded` on the application and service APIs, `a_cancelled_dispatch_is_discarded` on the CLA), abandonment of a collection (`an_abandoned_collection_defers_to_the_next_registration`) and of a forwarding (`an_abandoned_forwarding_stays_queued`), truncation by half-close (`a_truncated_send_never_commits`, `a_truncated_dispatch_never_commits`), withdrawal of an outbound transfer (`a_stream_that_ends_early_aborts_the_transfer`), the client vanishing mid-session on all four APIs (`a_dropped_stream_tears_the_session_down`), explicit `Unregister` on all four APIs, host pool shutdown (`pool_shutdown_tears_sessions_and_drains`, and `pool_shutdown_survives_a_claimed_unread_receive` with a claimed unread collection alive), client pool shutdown with a delivery blocked on its stream (`shutdown_ends_the_stream_a_delivery_is_blocked_on`), and teardown racing a blocked event push. Every abandonment test also asserts the deferral contract: the bundle stays parked, and the next announcement succeeds.
6. **The delivery commit is ack-gated, and pinned in every arm.** Completion is the client's ack, never the last chunk. A cancel after the final chunk, a full receipt followed by silence, an ack racing ahead of the final chunk (a protocol violation), a session death mid-collection, and an SDK handler that declines after buffering the whole ADU all leave the bundle parked and re-announced; only a conforming ack commits. The forwarding commit has the same shape on the CLA API: a result ahead of the last chunk is a violation, an `accepted` result parks the transfer until `ReportTransferOutcome`, and a `failed` outcome re-offers it.
7. **The send-to-self roundtrip is the smoke test of each payload API.** It proves the announce-and-collect pipeline live end to end, byte-for-byte where the API promises it: the service API returns the stored bundle exactly, while the application API compares the ADU and the CLA suite's `dispatch_and_forward_roundtrip` asserts the announced size and payload survival, because the BPA rewrites extension blocks at egress.
8. **Nothing is assumed that the server announces.** Every bound a client must meet is read back from the `Registration` rather than hard-coded in the test, and the announcement itself is pinned against the values the code ships with, so the documented numbers and the wire cannot drift apart. A negotiated chunk size is then held to on both directions: a chunk above it is refused, and a collection is cut at it. A CLA's declared `max_bundle_size` is echoed back and enforced on the declaration alone.
9. **Every door's opening is pinned.** For each call that opens with a message, a test sends nothing and gets the handshake timeout, sends the wrong message first and gets `INVALID_ARGUMENT` naming the expected one, or omits a required field and gets `INVALID_ARGUMENT` naming the field. The status message is asserted verbatim where it names a field, because the message is the only thing that tells two `INVALID_ARGUMENT` exits apart.

**Placement.** Tests that need no crate-private access are integration tests under `proto/tests/`, which is where 112 of the 166 live. The 54 that remain are inline `#[cfg(test)]` modules, each because it reads something private: the watchdog and the announce table are `pub(crate)` types with no wire representation; `chunking`, `token` and `timestamp` are private modules; `chunking::budget::Budget`, which every bounded transfer spends, is driven with scripted waits; `chunking::receiver::BoundedChunkReceiver`, the server's bounded reader, is driven over scripted request streams and read through its crate-private `into_error`; `chunking::sender::BoundedChunkSender`, the server's bounded sender, is driven directly with a held-back ending permit and scripted segment sources; `limits` computes the defaults and the session footprint from constants no client can see; the test in `lib.rs` pins the redacting `Debug` that `build.rs` configures on every generated message carrying a session token; and the six in `server/services/endpoint/application.rs` construct a `GrpcApplication` directly or read the `#[cfg(test)]` `opened`/`closed` hooks on `ApplicationServiceImpl`.

## 3. Test Suites

The suite IDs below are local to this document; a test's identity is its function name, and the inventory is regenerated with `cargo test -p hardy-proto --all-features -- --list`. Rows appear in file order, so an ID changes when a test is inserted before it.

### Suite UNT: pure units (`chunking/mod.rs`, `token.rs`, `timestamp.rs`, `lib.rs`), 17 tests

*Objective: pin the private modules whose edge cases the wire suites only reach by inference, and the one property of the generated types the crate adds itself.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **UNT-01** | `chunking::a_client_that_asks_for_nothing_runs_at_the_servers_chunk_size` | An absent `max_chunk_size` leaves the server its own `DEFAULT_CHUNK_SIZE`, so saying nothing is not the same as asking for zero | Implemented (`server` feature) |
| **UNT-02** | `chunking::an_ask_is_a_ceiling_and_never_a_floor` | A smaller ask is honoured and a larger one, up to `u64::MAX`, is held to `DEFAULT_CHUNK_SIZE`, so a client can only ever lower the size | Implemented (`server` feature) |
| **UNT-03** | `chunking::an_ask_below_the_minimum_is_refused` | An ask under `MIN_CHUNK_SIZE` (1 KiB) is `INVALID_ARGUMENT` naming `Register.max_chunk_size`, rather than answered above what was asked | Implemented (`server` feature) |
| **UNT-04** | `chunking::only_a_size_in_range_is_a_chunk_size` | `ChunkSize::new` admits `MIN_CHUNK_SIZE` through `MAX_CHUNK_SIZE` and nothing outside it, including zero and `u64::MAX`, so no later code re-checks the range | Implemented |
| **UNT-05** | `chunking::an_empty_intermediate_segment_yields_nothing` | An empty `Segment::Next` puts no chunk on the wire | Implemented |
| **UNT-06** | `chunking::an_empty_final_segment_still_ends_the_transfer` | An empty `Segment::Final` still yields its empty last chunk, so the end-of-transfer signal survives | Implemented |
| **UNT-07** | `chunking::a_segment_within_the_bound_is_one_chunk` | Under the chunk size, one chunk with the same bytes and the same finality | Implemented |
| **UNT-08** | `chunking::a_segment_of_exactly_the_bound_is_one_chunk` | At exactly the chunk size, one chunk and no empty trailer | Implemented |
| **UNT-09** | `chunking::only_the_last_chunk_of_a_final_segment_is_final` | A final segment of twice the chunk size plus 7 splits into three, only the trailing chunk final, bytes preserved in order | Implemented |
| **UNT-10** | `chunking::no_chunk_of_an_intermediate_segment_ends_the_transfer` | A split intermediate segment yields only `Next` chunks that concatenate back to the original | Implemented |
| **UNT-11** | `token::the_cleartext_prefix_cannot_size_the_token` | A 64 KiB subject yields a token of at most `MAX_SUB_LEN + 33` bytes, so a client-chosen name cannot size the token | Implemented (`server` feature) |
| **UNT-12** | `token::truncating_the_prefix_keeps_the_token_printable` | A multi-byte subject truncates on a character boundary, leaving the token valid UTF-8 | Implemented (`server` feature) |
| **UNT-13** | `token::identities_do_not_determine_the_token` | Two mints for one subject differ, so the random suffix and not the identity is what makes a token unguessable | Implemented (`server` feature) |
| **UNT-16** | `token::a_token_longer_than_a_minted_one_is_not_presentable` | A token minted for the longest allowed subject is exactly `MAX_TOKEN_LEN`, and a presented token one byte longer is refused before it is read, so a client cannot size the work of checking it | Implemented (`server` feature) |
| **UNT-17** | `token::a_token_is_only_equal_to_itself` | A token equals its own clone, two mints for one subject differ, and a prefix of a token is not the token, so no partial match passes for a presented token | Implemented (`server` feature) |
| **UNT-14** | `timestamp::an_unrepresentable_timestamp_is_skipped_not_panicked_on` | A `Timestamp` that overflows `OffsetDateTime` converts to `None` rather than panicking, while a representable one keeps its nanoseconds | Implemented (`client` feature) |
| **UNT-15** | `tests::a_message_carrying_the_token_does_not_print_it` (`lib.rs`) | The `Debug` of `SendMetadata`, `Registration`, `AddPeerRequest` and `AddRouteRequest` prints `token(34 bytes)` and never the token's bytes, so a session token cannot reach a log through a message's `Debug` | Implemented |

### Suite WDG: the watchdog (`server/watchdog.rs`), 4 tests

*Objective: pin the first-stall-wins record and the wake-up it gives the session, over a paused clock.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **WDG-01** | `the_first_stall_is_the_one_recorded` | An expired `idle(Ack)` returns `DEADLINE_EXCEEDED` with the message `timed out waiting for the client`, `stalled()` then reads `Ack`, and a later expired `claim()` returns the same status to its own caller without overwriting the recorded stage | Implemented |
| **WDG-02** | `a_stall_wakes_a_waiting_session` | A waiter on `stalled()` is woken with the stage of a concurrent `idle()` that expires, which itself returns the `DEADLINE_EXCEEDED` status | Implemented |
| **WDG-03** | `a_recorded_stall_wakes_a_late_waiter` | A `stalled()` call made after the stall resolves immediately with the recorded stage | Implemented |
| **WDG-04** | `a_quiet_watchdog_never_wakes_a_waiter` | `stalled()` stays pending for an hour on a watchdog whose bounding methods were never called | Implemented |

### Suite BDG: the transfer budget (`chunking/budget.rs`), 6 tests

*Objective: pin the idle ceiling and the rate floor every bounded transfer is built on, over a paused clock, with the waits scripted directly.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **BDG-01** | `a_drip_feeder_is_stalled_once_its_grace_is_spent` | One byte per second survives exactly `grace` (30 seconds) and is then refused, so earned time is `moved / min_rate` | Implemented |
| **BDG-02** | `a_client_sustaining_the_minimum_rate_is_never_stalled` | A client moving exactly `min_rate` bytes per wait survives 600 waits, so the floor never catches a client that meets it | Implemented |
| **BDG-03** | `a_disabled_rate_floor_leaves_only_the_idle_rule` | With `min_rate: None` sixty 29 second waits all survive, so disabling the floor leaves `idle` as the only bound and never shortens it | Implemented |
| **BDG-04** | `a_client_that_goes_quiet_is_caught_by_the_idle_ceiling` | A client credited with 1 GiB is still refused a 31 second wait, so earned time can only tighten `idle`, never extend it | Implemented |
| **BDG-05** | `the_servers_own_time_is_not_charged_to_the_client` | Sixty minutes of the server's own work between prompt answers spend none of the budget, so only the wait on the client is charged | Implemented |
| **BDG-06** | `the_waits_of_a_transfer_are_charged_together` | Three ten second waits fit a 30 second grace and a fourth does not, so the budget belongs to the transfer and not to each wait | Implemented |

### Suite RCV: the bounded reader (`chunking/receiver.rs`), 9 tests

*Objective: pin what the server's bounded reader adds over the budget: the chunk-size, empty-chunk and declared-size checks, and the ending it keeps for its caller, driven over a real request stream without a network.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **RCV-01** | `a_drip_feeding_client_is_stalled_once_its_grace_is_spent` | A client sending one byte a second over a request stream hands over exactly 30 segments and is then stalled, with `into_error()` reporting `TimedOut`, so the budget reaches the wire path intact | Implemented |
| **RCV-02** | `a_prompt_client_does_not_pay_for_the_bpa_s_time` | Sixty segments answered the moment they are asked for all survive, though the BPA takes a minute over each, so the client is charged only for its own silence | Implemented |
| **RCV-03** | `a_client_that_stops_feeding_is_stalled_and_the_stall_is_kept` | A client that sends one segment and then goes quiet hands that segment over, fails `recv`, and `into_error()` returns `TimedOut` as this layer's own ending | Implemented |
| **RCV-04** | `a_stream_that_closes_early_is_reported_when_no_bound_was_crossed` | A request stream that closes before the last chunk fails `recv`, and `into_error()` returns `ABORTED` with the message `request stream closed before the last chunk` | Implemented |
| **RCV-05** | `a_chunk_above_the_agreed_size_ends_the_transfer` | On a receiver agreed at 4096, a 4097 byte segment fails `recv`, and `into_error()` returns `RESOURCE_EXHAUSTED` with the message `a chunk of 4097 bytes exceeds the agreed 4096 bytes` | Implemented |
| **RCV-06** | `an_empty_chunk_before_the_last_ends_the_transfer` | An empty non-final segment fails `recv`, and `into_error()` returns `INVALID_ARGUMENT` with the message `only the last chunk of a transfer may be empty`, so a transfer cannot run forever without growing | Implemented |
| **RCV-07** | `an_empty_last_chunk_ends_the_transfer_normally` | An empty `Final` segment is handed over and `into_error()` is `None`, so the empty-chunk rule leaves the end-of-transfer signal alone | Implemented |
| **RCV-08** | `a_transfer_is_held_to_its_declared_size` | With 5 bytes declared, a second four-byte segment fails `recv` as soon as the count passes the declaration, before the final segment is judged, and `into_error()` returns `INVALID_ARGUMENT` with the message `the transfer declared 5 bytes, more arrived` | Implemented |
| **RCV-09** | `a_declaration_above_the_transfer_limit_is_refused_before_reading` | `BoundedChunkReceiver::new` with a declaration of `MAX_TRANSFER_SIZE + 1` fails `RESOURCE_EXHAUSTED` before any segment is read | Implemented |

### Suite RVZ: the rendezvous primitives (`server/announce.rs`, `chunking/sender.rs`), 10 tests

*Objective: pin the announce-and-collect table the delivery and forward rendezvous share, and the server's bounded writer, without a network.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **RVZ-01** | `announce::a_collect_is_single_use` | An announced entry is collectable exactly once; a second `take` of the same id misses | Implemented |
| **RVZ-02** | `announce::an_abandoned_announcement_frees_its_id` | An announcer that goes away leaves an entry that hands nothing over, and the next announcement of the same id takes its place, so a collection reaches the live announcement | Implemented |
| **RVZ-03** | `announce::a_duplicate_announcement_is_refused` | A second announcement of a live id fails `AlreadyAnnounced` rather than replacing the entry, leaving the live one collectable | Implemented |
| **RVZ-04** | `announce::an_announcement_the_client_is_never_told_of_is_withdrawn` | An offer the session stream will not carry fails `SessionClosed` and withdraws its entry, so a bundle nobody was told about is not left blocking its own re-announcement | Implemented |
| **RVZ-05** | `announce::an_uncollected_announcement_stalls_at_the_claim_stage` | With `claim` zero an offer no door answers fails `CollectionTimedOut` and the watchdog records the `claim` stage, so the table itself bounds the wait no server can forget to bound | Implemented |
| **RVZ-06** | `sender::a_stream_that_ends_early_aborts_the_transfer` | A source that ends before its final chunk breaks the transfer with `ABORTED`, and the writer queues nothing: the status is the exchange's to send through the slot it holds | Implemented |
| **RVZ-07** | `sender::a_client_that_stops_reading_stalls_at_the_drain_stage` | With the ending permit held back, `BUFFERED_CHUNKS` chunks go out without reaching the watchdog; the next one, with nowhere to go, breaks `DEADLINE_EXCEEDED` under a zero bound, the queued chunks are intact, and the writer queued no status of its own | Implemented |
| **RVZ-08** | `sender::a_client_that_leaves_the_tail_unread_stalls_at_the_drain_stage` | A last chunk the client never takes breaks `send_all` as `DEADLINE_EXCEEDED`, with the last chunk itself sent, so completion is the client's read of the tail and not the send of it | Implemented |
| **RVZ-09** | `sender::a_transfer_completes_once_the_client_has_taken_the_last_chunk` | A head and a tail read by a concurrent reader complete `send_all` with `Ok`, arrive as a `Chunk` then a `LastChunk` with the right bytes, and nothing follows them on the sender's side | Implemented |
| **RVZ-10** | `announce::a_collection_racing_a_withdrawal_cannot_win_the_hand_over` | A collection that takes the hand-over of an announcement which then ends finds its channel already closed, so a bundle is never handed to a door whose announcer has gone | Implemented |

### Suite SES: the session's event path (inline in `server/services/endpoint/application.rs`), 6 tests

*Objective: pin the event-push and slot invariants every server's copy is written from, with the handler constructed directly.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **SES-01** | `an_event_after_the_session_closes_is_refused` | Teardown cancels the session token, and the biased race refuses events `UNAVAILABLE` (`registration closed`) even with buffer space free, queuing nothing | Implemented |
| **SES-02** | `an_event_after_the_session_is_retired_is_refused` | The handler's sender is weak: once the session task has dropped the strong end, a door still holding the handler reports `UNAVAILABLE` rather than panicking | Implemented |
| **SES-03** | `an_event_blocked_on_a_full_buffer_is_freed_by_teardown` | A push parked on a full buffer is released by cancellation alone, with no timer and no reader, and refused `UNAVAILABLE`, so a slow consumer cannot wedge a dying session | Implemented |
| **SES-04** | `an_event_the_client_leaves_no_room_for_stalls_the_session` | With `idle` zero an event the client leaves no room for breaks `DEADLINE_EXCEEDED`, and the watchdog records the `event` stage, which closes the session behind it | Implemented |
| **SES-05** | `an_unregistered_subscription_does_not_block_shutdown` | A subscription parked on a `Register` that never arrives is ended `UNAVAILABLE` by pool shutdown, so a silent client cannot hold shutdown open | Implemented |
| **SES-06** | `a_closed_session_returns_its_slot` | With a ceiling of one, a closed session releases its slot and the next registration succeeds, so the ceiling is a live-session bound and not a lifetime quota | Implemented |

### Suite LIM: the server's bounds (`limits.rs`), 3 tests

*Objective: pin the shipped defaults and the per-session footprint the ceiling is read against.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **LIM-01** | `the_defaults_are_the_ones_documented` | The shipped defaults are a 10 second handshake, 30 second idle, claim and grace bounds, a 1024 byte per second floor and 64 sessions, so the documentation and the code cannot drift apart | Implemented |
| **LIM-02** | `the_session_footprint_is_the_one_documented` | `SESSION_FOOTPRINT` is `MAX_INBOUND_TRANSFERS` transfers of one decoded message plus one chunk, plus a collection's `BUFFERED_CHUNKS + 1` chunks: 4 × 3 MiB + 5 MiB, and `CLA_SESSION_FOOTPRINT` carries four such collections, so the documented per-session budgets are the ones derived from the constants | Implemented |
| **LIM-03** | `the_server_only_decodes_what_a_session_can_carry` | `MAX_SERVER_MESSAGE_SIZE` is 2 MiB and stays above `DEFAULT_CHUNK_SIZE`, so a chunk and the rest of its fields still fit in one message while the decode cap the footprint is derived from is the one the server applies | Implemented |

### Suite STS: the status mapping (`server/status.rs`), 0 tests

*Objective: pin that each BPA error becomes the status its call ends with. `service_status`, `cla_status` and `routing_status` are plain many-to-one mappings onto a gRPC code and a short message; no detail or reason rides with the status.*

No test drives the mapping functions directly. The arms a wire scenario reaches are exercised through it (`NodeId` by APP-17, `Disconnected` by APP-12, `StreamCancelled` by APP-04, `InvalidBundle` by SVC-06, `InvalidDestination` by SVC-09, `AlreadyExists` by CLA-01 and RTE-01); the rest have no test.

| Test ID | Scenario | What it would pin | Status |
| :--- | :--- | :--- | :--- |
| **STS-01** | Every arm of the three mapping functions | Each BPA error maps to the documented code; the message names the field or the counts and never the client's own input; `Internal` answers the fixed `internal error`; the arms the BPA never constructs (`DtnInvalidServiceName`, `NoIpnNodeId`, `NoDtnNodeId`, see [TODO.md](TODO.md)) mint no contract of their own | Not implemented |

### Suite APP: application API (`tests/application.rs`), 46 tests

*Objective: verify the `hardy.application.v1` wire against a real BPA, including the send accumulation scaffold, the declared size, the negotiated chunk size, the numbered event stream, the ack-gated commit, the parked-delivery semantics, the opening of every call, and the stall defences.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **APP-01** | `explicit_and_dynamic_registrations_mint_distinct_sessions` | An explicit ipn registration resolves to `ipn:1.7`; a dynamic one gets a different endpoint and a different token | Implemented |
| **APP-02** | `send_to_self_roundtrip` | Send returns a bundle id, the `Delivery` announces that id with the session's own endpoint as source and the ADU size, the collection returns the ADU, and a second collect is `NOT_FOUND` | Implemented |
| **APP-03** | `a_truncated_send_never_commits` | Half-close after a non-final chunk is `ABORTED` and nothing is delivered | Implemented |
| **APP-04** | `a_cancelled_send_is_discarded` | An in-band `cancel` mid-send is `CANCELLED` and nothing is delivered | Implemented |
| **APP-05** | `receive_of_a_malformed_id_is_invalid_argument` | A `Receive` naming an unparseable bundle id is `INVALID_ARGUMENT` at the door | Implemented |
| **APP-06** | `an_abandoned_collection_defers_to_the_next_registration` | An in-band cancel mid-collection ends the call cleanly, the spent id answers `NOT_FOUND`, and the next registration collects the whole ADU | Implemented |
| **APP-07** | `a_forged_token_is_rejected` | A token the server never minted is `UNAUTHENTICATED` | Implemented |
| **APP-08** | `a_dropped_stream_tears_the_session_down` | Dropping the rpc without `Unregister` runs the BPA's `on_unregister` and invalidates the token | Implemented |
| **APP-09** | `pool_shutdown_tears_sessions_and_drains` | Host pool shutdown ends live session streams with `UNAVAILABLE` and the shutdown itself completes | Implemented |
| **APP-10** | `an_empty_adu_delivers_end_to_end` | An empty ADU is announced with size 0 and collects empty, never as a truncation, and a second collect is `NOT_FOUND` | Implemented |
| **APP-11** | `a_receive_before_the_announcement_is_not_found` | A `Receive` for an id not yet announced is `NOT_FOUND`, and that early miss neither consumes nor poisons the later announcement of the same id | Implemented |
| **APP-12** | `session_death_mid_receive_defers_the_delivery` | Unregistering with a 16 MiB collection in flight ends it `UNAVAILABLE` (`registration closed`) before any `LastChunk`, and the next registration collects it whole | Implemented |
| **APP-13** | `a_cancel_after_the_last_chunk_parks_the_delivery` | Completion is the ack, not the last chunk: a cancel after the final chunk ends cleanly and the bundle is re-announced to the next registration | Implemented |
| **APP-14** | `a_full_receipt_without_an_ack_parks_the_delivery` | A client that takes the whole ADU and then drops its request stream gets `CANCELLED` and commits nothing; the bundle is re-announced | Implemented |
| **APP-15** | `pool_shutdown_survives_a_claimed_unread_receive` | A claimed 16 MiB collection nobody reads does not wedge pool shutdown | Implemented |
| **APP-16** | `a_client_that_stops_feeding_stalls_at_the_feed_stage` | With `grace` zero, a `Send` silent after its metadata ends `DEADLINE_EXCEEDED` at `feed`, which unregisters the session | Implemented |
| **APP-17** | `a_dtn_registration_needs_a_dtn_node_id` | A `dtn`-scheme registration on an ipn-only node fails the handshake `FAILED_PRECONDITION` | Implemented |
| **APP-18** | `a_dtn_registration_binds_the_dtn_endpoint` | On a node declaring a `dtn` node id, the same registration binds `dtn://node1/mail` | Implemented |
| **APP-19** | `the_sdk_reports_a_missing_node_id_as_such` | `BpaClient::register_application` with a `dtn` service on an ipn-only node fails with `services::Error::NoDtnNodeId`, so the SDK names the missing node id rather than a generic failure | Implemented (`client` feature) |
| **APP-20** | `client_sdk_roundtrip` | An application behind `BpaClient` registers as `ipn:1.9` and its send to itself arrives in `on_deliver` with the identical payload and its own endpoint as source | Implemented (`client` feature) |
| **APP-21** | `an_sdk_decline_after_full_receipt_is_redelivered` | An SDK app returning `Err` from `on_deliver` after reading the payload sends no ack; the bundle stays parked and the next registration receives it | Implemented (`client` feature) |
| **APP-22** | `a_delivery_report_reaches_the_sending_application` | A send with `notify_delivery` produces an `on_status_notify` of `Delivered` for the bundle id `send` returned | Implemented (`client` feature) |
| **APP-23** | `re_registration_re_announces_many_parked_deliveries` | 48 parked deliveries are re-announced with distinct ids after re-registration, and the last one collects | Written, `#[ignore]`d: needs per-delivery concurrency in the BPA ([TODO.md](TODO.md)) |
| **APP-24** | `unregister_ends_the_session_and_invalidates_the_token` | An explicit `Unregister` ends the stream cleanly, with no status, and the token dies with the session | Implemented |
| **APP-25** | `an_rpc_abandoned_during_registration_ends_unregistered` | An rpc abandoned while the BPA is committing the registration still ends unregistered: the session runs on a task the client cannot cancel, so the commit completes and is then undone | Implemented |
| **APP-26** | `an_uncollected_delivery_expires_its_claim_and_ends_the_session` | With `claim` zero, a delivery the client never collects stalls and unregisters the session | Implemented |
| **APP-27** | `a_delivery_left_unacked_stalls_at_the_ack_stage_and_ends_the_session` | With `idle` zero and the ADU originated in-process by `SinkOnly`, a collection drained to its last chunk and left unacked ends `DEADLINE_EXCEEDED` at `ack` with the whole ADU received, and the session closes | Implemented |
| **APP-28** | `a_declared_adu_size_above_the_bound_is_rejected_preflight` | A declared size of `MAX_TRANSFER_SIZE + 1` is `RESOURCE_EXHAUSTED` on the metadata alone, before any ADU byte arrives | Implemented |
| **APP-29** | `a_transfer_that_misses_its_declared_size_is_refused` | A declaration is binding in both directions: one byte short and one byte over both fail `INVALID_ARGUMENT`, with a message naming the declared count and whether more or fewer arrived | Implemented |
| **APP-30** | `a_transfer_that_matches_its_declared_size_is_accepted` | An exact declaration commits, so the check costs a conforming client nothing | Implemented |
| **APP-31** | `an_ack_before_the_final_chunk_never_commits` | An ack racing the drain is a protocol violation: the call fails `INVALID_ARGUMENT` (`ack before the last chunk`) before any `LastChunk`, commits nothing, and the bundle is re-collectable after re-registration | Implemented |
| **APP-32** | `an_api_at_its_ceiling_refuses_a_further_session` | With a ceiling of one, a second `Subscribe` is refused `RESOURCE_EXHAUSTED` rather than queued | Implemented |
| **APP-33** | `an_uncollected_delivery_on_one_session_does_not_block_another` | A session sitting on an announced delivery it never collects does not stop another session sending, being announced, and collecting | Implemented |
| **APP-34** | `an_sdk_reply_from_within_a_delivery_does_not_deadlock` | An SDK app that replies through its own sink from inside `on_deliver` handles 32 echoes without self-deadlock, which is the echo-over-gRPC shape | Implemented (`client` feature) |
| **APP-35** | `a_registration_announces_the_sizes_the_session_runs_at` | The `Registration` carries `MAX_MESSAGE_SIZE`, `DEFAULT_CHUNK_SIZE` and `MAX_TRANSFER_SIZE`, the values the server will actually apply | Implemented |
| **APP-36** | `a_chunk_size_the_client_asks_for_is_the_one_announced` | A `Register.max_chunk_size` of 4096 comes back as the announced `chunk_size`, so the negotiation is the one the session runs at | Implemented |
| **APP-37** | `a_chunk_size_below_the_minimum_is_refused` | A `Register.max_chunk_size` of one byte fails the `Subscribe` with `INVALID_ARGUMENT`, before a session or a slot exists | Implemented |
| **APP-38** | `a_chunk_above_the_negotiated_size_is_refused` | On a session negotiated at 4096, a 4097 byte chunk fails the `Send` `RESOURCE_EXHAUSTED` with a message naming both sizes, so the negotiated size binds the client as well as the server | Implemented |
| **APP-39** | `a_receive_is_chunked_at_the_negotiated_size` | On a session negotiated at 4096, an ADU of three chunks plus five bytes arrives in more than one chunk, none above 4096, concatenating back to the ADU | Implemented |
| **APP-40** | `a_subscribe_that_never_registers_times_out_the_handshake` | With `handshake` zero, a `Subscribe` that sends nothing ends `DEADLINE_EXCEEDED` with the message `timed out waiting for Register` | Implemented |
| **APP-41** | `a_send_that_never_sends_metadata_times_out_the_handshake` | With `handshake` zero, a `Send` that sends nothing ends `DEADLINE_EXCEEDED` with the message `timed out waiting for SendMetadata` | Implemented |
| **APP-42** | `a_send_opened_with_a_chunk_is_invalid_argument` | A `Send` whose first message is a chunk is `INVALID_ARGUMENT` naming `SendMetadata` as the message that must open the call | Implemented |
| **APP-43** | `a_subscribe_opened_with_unregister_is_invalid_argument` | A `Subscribe` whose first message is `Unregister` is `INVALID_ARGUMENT` naming `Register` as the message that must open the call | Implemented |
| **APP-44** | `a_send_without_a_lifetime_is_invalid_argument` | A `SendMetadata` with no `lifetime` is `INVALID_ARGUMENT` naming `SendMetadata.lifetime` | Implemented |
| **APP-45** | `a_send_to_a_destination_that_is_not_an_eid_is_invalid_argument` | A `SendMetadata` whose `destination` does not parse is `INVALID_ARGUMENT` naming `SendMetadata.destination` | Implemented |
| **APP-46** | `a_stray_message_on_the_session_stream_is_ignored` | A second `Register` on a live session stream is ignored: a send still delivers, and the `Unregister` that follows it ends the stream cleanly, which the ordered request stream makes proof that the stray was not a fault | Implemented |

### Suite SVC: service API (`tests/service.rs`), 16 tests

*Objective: verify the `hardy.service.v1` wire, including the native streamed send path and the BPA's validation at the trust boundary.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **SVC-01** | `explicit_and_dynamic_registrations_mint_distinct_sessions` | As APP-01, for the service API | Implemented |
| **SVC-02** | `send_to_self_roundtrip` | A whole bundle round-trips byte-identical through send, delivery and collection, with the announced size and id matching; a second collect is `NOT_FOUND` | Implemented |
| **SVC-03** | `an_uncollected_delivery_expires_its_claim_and_ends_the_session` | As APP-26 | Implemented |
| **SVC-04** | `a_truncated_send_never_commits` | As APP-03, through the streamed writer | Implemented |
| **SVC-05** | `a_cancelled_send_is_discarded` | As APP-04 | Implemented |
| **SVC-06** | `an_invalid_bundle_is_rejected` | Bytes that are not a valid bundle fail BPA validation with `INVALID_ARGUMENT`; nothing enters the store | Implemented |
| **SVC-07** | `an_abandoned_collection_defers_to_the_next_registration` | As APP-06, for whole bundles | Implemented |
| **SVC-08** | `a_forged_token_is_rejected` | As APP-07 | Implemented |
| **SVC-09** | `a_forged_source_is_rejected` | A bundle claiming an endpoint other than the session's own as its source is `INVALID_ARGUMENT` at the trust boundary | Implemented |
| **SVC-10** | `a_dropped_stream_tears_the_session_down` | As APP-08 | Implemented |
| **SVC-11** | `client_sdk_roundtrip` | A service behind `BpaClient` registers as `ipn:1.9` and its own bundle arrives byte-identical in `on_deliver` | Implemented (`client` feature) |
| **SVC-12** | `a_delivery_report_reaches_the_sending_service` | A service-built bundle requesting a delivery report gets an `on_status_notify` of `Delivered` for the id `send` returned, which is the id the builder produced | Implemented (`client` feature) |
| **SVC-13** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-24 | Implemented |
| **SVC-14** | `the_negotiated_chunk_size_is_announced_and_held_to` | A registration asking for 4096 is announced at 4096 and a 4097 byte chunk then fails `RESOURCE_EXHAUSTED`, so the service API negotiates and enforces the chunk size as the application API does | Implemented |
| **SVC-15** | `a_bundle_that_misses_its_declared_size_is_refused` | A `bundle_size` one byte under the bundle and one byte over it both fail `INVALID_ARGUMENT`, each message naming the declaration and what arrived | Implemented |
| **SVC-16** | `a_bundle_that_matches_its_declared_size_is_accepted` | A bundle declaring its exact size is stored and answered with its id, so a declaration is a check on the transfer and not a second length to agree | Implemented |

### Suite CLA: convergence-layer API (`tests/cla.rs`), 27 tests

*Objective: verify the `hardy.cla.v1` wire, including the `Forward` rendezvous, the single-executor claim, the deferred outcome and its two verdicts, the peer doors, the registration-time validation, and the declared bundle size.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **CLA-01** | `registration_returns_node_ids_and_a_token` | Registration returns the BPA's node ids (`ipn:1.0`) and a non-empty token; a duplicate CLA name fails `ALREADY_EXISTS` | Implemented |
| **CLA-02** | `dispatch_and_forward_roundtrip` | A dispatched bundle for a destination behind an announced peer is answered `ACCEPTANCE_ACCEPTED`, announced as a `Forwarding` carrying the peer address, and streamed back whole, its length the announced size and its payload intact | Implemented |
| **CLA-03** | `an_abandoned_forwarding_stays_queued` | An in-band cancel mid-`Forward` ends the call cleanly with no result, the BPA requeues, and the re-announced forwarding completes | Written, `#[ignore]`d: needs the BPA to retry a synchronous `Cla::forward` failure by its kind ([TODO.md](TODO.md)) |
| **CLA-04** | `a_completed_outcome_finishes_an_accepted_forwarding` | An `accepted` result parks the transfer for `ReportTransferOutcome`; a `completed` outcome finishes it, and a later `failed` for the finished transfer is accepted and re-queues nothing: the next dispatched bundle is the next one announced, in a peer queue that offers in order | Implemented |
| **CLA-05** | `a_failed_outcome_re_offers_an_accepted_forwarding` | A `failed` outcome for an accepted transfer re-announces the same bundle id, which then executes whole | Implemented |
| **CLA-06** | `an_outcome_without_a_verdict_is_invalid_argument` | A `ReportTransferOutcome` carrying no outcome is `INVALID_ARGUMENT` naming `ReportTransferOutcomeRequest.outcome` | Implemented |
| **CLA-07** | `an_add_peer_without_an_address_is_invalid_argument` | An `AddPeer` carrying no address is `INVALID_ARGUMENT` naming `AddPeerRequest.address` | Implemented |
| **CLA-08** | `a_dispatch_naming_an_invalid_peer_node_id_is_invalid_argument` | A `DispatchMetadata` whose `peer_node_id` does not parse is `INVALID_ARGUMENT` naming the field | Implemented |
| **CLA-09** | `an_empty_result_ends_the_forwarding` | A `ForwardResult` carrying no inner result is `INVALID_ARGUMENT` naming `ForwardResult.result` rather than a wait, so it cannot park the egress queue, and the bundle is forwardable again after re-registration | Implemented |
| **CLA-10** | `an_unknown_address_type_is_refused` | An out-of-range declared `address_type` is `INVALID_ARGUMENT` and leaves the CLA name free | Implemented |
| **CLA-11** | `an_uncollected_forwarding_expires_its_claim_and_ends_the_session` | With `claim` zero, a forwarding the CLA never collects stalls and unregisters the session | Implemented |
| **CLA-12** | `a_truncated_dispatch_never_commits` | A dispatch ending after a non-final chunk is `ABORTED` and no forwarding is announced | Implemented |
| **CLA-13** | `a_cancelled_dispatch_is_discarded` | An in-band cancel mid-dispatch is `CANCELLED` and no forwarding is announced | Implemented |
| **CLA-14** | `peers_are_added_and_removed_once` | `AddPeer` and `RemovePeer` are idempotent: each reports its change only the first time | Implemented |
| **CLA-15** | `a_forged_token_is_rejected` | As APP-07, on a unary door | Implemented |
| **CLA-16** | `a_dropped_stream_tears_the_session_down` | Dropping the rpc unregisters the CLA, invalidates its token, and frees the name | Implemented |
| **CLA-17** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-24 | Implemented |
| **CLA-18** | `a_forward_for_a_malformed_bundle_id_is_invalid_argument` | A `Forward` whose metadata carries an unparseable bundle id is `INVALID_ARGUMENT` | Implemented |
| **CLA-19** | `forward_requires_the_metadata_first` | A `Forward` whose first message is a result rather than the metadata is `INVALID_ARGUMENT` | Implemented |
| **CLA-20** | `a_duplicate_forward_for_a_live_call_is_not_found` | The claim is single-executor: a duplicate `Forward` for an id a live call already holds is `NOT_FOUND`, and the live call completes untouched with the whole bundle | Implemented |
| **CLA-21** | `an_sdk_deferred_outcome_completes_the_transfer` | A CLA behind `BpaClient` answering `Accepted` from `forward` and later reporting `Completed` through its sink completes the transfer, so the bundle is never re-offered | Implemented (`client` feature) |
| **CLA-22** | `client_sdk_roundtrip` | A CLA behind `BpaClient` announces a peer, dispatches, and receives the bundle back through `Cla::forward` with its payload intact | Implemented (`client` feature) |
| **CLA-23** | `an_early_sent_never_completes_the_forwarding` | A `sent` claimed before the last chunk is read is `INVALID_ARGUMENT` (`result before the last chunk`) and never completes the forwarding, over a payload too large for the buffers to absorb; the bundle is forwardable again after re-registration | Implemented |
| **CLA-24** | `lane_count_is_validated_at_registration` | A declared lane count of zero or above `MAX_LANE_COUNT` is `INVALID_ARGUMENT` at the wire boundary; the bound itself registers | Implemented |
| **CLA-25** | `the_sdk_rejects_an_over_declared_lane_count` | The SDK surfaces the registration-time rejection of an over-declared lane count as `cla::Error::Internal` | Implemented (`client` feature) |
| **CLA-26** | `an_unspecified_address_type_is_invalid_argument` | A `Register` whose `address_type` is `UNSPECIFIED` is `INVALID_ARGUMENT` with a message naming the field, so the enum's zero value is not read as a choice | Implemented |
| **CLA-27** | `a_dispatch_declared_above_the_registration_limit_is_refused_before_any_byte` | A `Register.max_bundle_size` of 1024 is echoed in the `Registration`, and a `DispatchMetadata` declaring 1025 bytes is `RESOURCE_EXHAUSTED` with a message naming both numbers, on the declaration alone and before any chunk is read | Implemented |

### Suite RTE: routing API (`tests/routing.rs`), 10 tests

*Objective: verify the `hardy.routing.v1` wire: the push-only session and the two unary route doors.*

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **RTE-01** | `registration_returns_node_ids_and_a_token` | Registration returns the BPA's node ids and a non-empty token; a duplicate agent name fails `ALREADY_EXISTS` | Implemented |
| **RTE-02** | `routes_are_added_and_removed_once` | `AddRoute` and `RemoveRoute` are idempotent against the real RIB for one pattern, action and priority | Implemented |
| **RTE-03** | `an_invalid_pattern_is_rejected` | An unparseable EID pattern is `INVALID_ARGUMENT` | Implemented |
| **RTE-04** | `a_missing_action_is_rejected` | A route with no action is `INVALID_ARGUMENT` | Implemented |
| **RTE-05** | `a_reserved_drop_reason_is_rejected` | RFC 9171's reserved reason code 255 in a `drop` action is `INVALID_ARGUMENT`, while an unassigned code is carried through | Implemented |
| **RTE-06** | `a_forged_token_is_rejected` | As APP-07 | Implemented |
| **RTE-07** | `a_dropped_stream_tears_the_session_down` | Dropping the rpc unregisters the agent, invalidates its token, and frees the name | Implemented |
| **RTE-08** | `unregister_ends_the_session_and_invalidates_the_token` | As APP-24 | Implemented |
| **RTE-09** | `client_sdk_roundtrip` | A routing agent behind `BpaClient` reads its node id, drives add and remove idempotence through its sink, and has the sink refuse the reserved drop reason, as `RouteActionError::ReservedReason`, before it reaches the wire | Implemented (`client` feature) |
| **RTE-10** | `a_route_decides_which_peer_a_bundle_leaves_by` | A bundle for a node no peer owns leaves by the peer the added route names, and by the other peer once that route is replaced, so a route installed over the wire is what decides delivery in the real RIB | Implemented |

## 4. Execution Strategy

Building the crate requires `protoc`, because the schemas compile in `build.rs`. The `default` build is the wire contract alone: it runs one test, UNT-15, which only needs the generated types, and no doctests.

- `cargo test -p hardy-proto --features server` runs 141 tests plus 3 doctests, of which 2 are `#[ignore]`d: 53 in the library (UNT-14 is `client`-only), 41 in `tests/application.rs`, 24 in `tests/cla.rs`, 9 in `tests/routing.rs`, 14 in `tests/service.rs`, and none in `tests/lifecycle.rs`, which needs both features. It leaves out UNT-14 and the 11 `client`-gated tests in the four wire suites (APP-19 to APP-22 and APP-34, CLA-21, CLA-22 and CLA-25, SVC-11 and SVC-12, RTE-09).
- `cargo test -p hardy-proto --all-features` runs the full inventory: 166 tests plus 5 doctests, of which 3 are `#[ignore]`d.
- CI runs the workspace with `--all-features`, so the full inventory runs on every change.

Every wire test uses the multi-threaded runtime (`worker_threads = 2`), because a single-threaded runtime would serialise the server, the client, and the BPA's dispatcher tasks against each other. That includes the zero-limit stall tests, with two exceptions that are deterministic on the current-thread runtime: APP-26 (the `claim` stage) and APP-27 (the `ack` stage). The inline units run current-thread, apart from SES-05 and SES-06, which mount a server, and the watchdog suite, the budget suite and RCV-01 to RCV-03, which run on a paused clock.

## 5. The cross-crate lifecycle suite

### Suite LIF: SDK lifecycle (`tests/lifecycle.rs`), 13 tests

The lifecycle scenarios drive `BpaClient` against a served application API through the crate's public API only, so they need both features. `LifecycleApp` is one test component with four delivery modes (collect, decline, stall on the stream past the last chunk, and rendezvous on a barrier), reporting its observations over a channel. `killable_proxy` interposes a TCP proxy a test can kill, which is what lets LIF-08 assert a transport failure rather than a close.

| Test ID | Test function | What it pins | Status |
| :--- | :--- | :--- | :--- |
| **LIF-01** | `a_client_unregister_round_trips` | A client `Unregister` ends the session, the SDK surfaces `on_unregister`, the registration handle resolves `Ok`, and the service id frees for a successor | Implemented |
| **LIF-02** | `bpa_initiated_teardown_reaches_the_client` | Shutting the BPA down unregisters the server's component, ends the wire session, surfaces `on_unregister`, and resolves the handle `Err(Disconnected)`: nobody on this side asked for the ending | Implemented |
| **LIF-03** | `a_client_pool_shutdown_defers_a_declined_delivery` | A delivery an application declined, whose client pool is then shut down, is re-announced intact to the endpoint's next registration from a fresh client | Implemented |
| **LIF-04** | `simultaneous_unregister_settles` | Unregistration from both ends converges with neither side hanging and exactly one observed `on_unregister` | Implemented |
| **LIF-05** | `dropping_the_sink_unregisters` | An application that never stores its sink has disconnected by definition: the dropped sink half-closes the session, the server unregisters it, and the handle resolves `Ok` because the drop was this side asking | Implemented |
| **LIF-06** | `a_client_shutdown_releases_the_registration` | Shutting the client's `TaskPool` down unregisters the application, ends the handle cleanly, and releases the registration for a new client | Implemented |
| **LIF-07** | `a_server_pool_shutdown_disconnects_the_client` | Shutting the server's `TaskPool` down surfaces `on_unregister`, resolves the handle `Err(Disconnected)`, and makes a later send on the orphaned sink fail `Disconnected` rather than block | Implemented |
| **LIF-08** | `a_transport_loss_surfaces_the_session_error` | A connection killed without trailers is a stream failure, not a close: the handle yields `Internal` wrapping the transport's own `Status`, code `Unknown`, with its source chain intact, and the SDK still surfaces `on_unregister` | Implemented |
| **LIF-09** | `shutdown_ends_the_stream_a_delivery_is_blocked_on` | An `on_deliver` blocked reading past the last chunk, where the server holds the stream open for the ack, is ended by client pool shutdown, and the session still runs its unregistration to completion | Implemented |
| **LIF-10** | `deliveries_collect_concurrently` | Two announced bundles are inside `on_deliver` at once, so a slow collection does not stall the next announcement | Written, `#[ignore]`d: needs per-delivery concurrency in the BPA ([TODO.md](TODO.md)) |
| **LIF-11** | `a_dead_session_fails_a_send_from_a_stalled_producer` | A streamed request answered before its request stream is read returns that answer however slow the local producer, so a dead session's send fails `Disconnected` even though its producer never yields a segment. Pins the SDK's streamed writer, shared by all three streamed request calls | Implemented |
| **LIF-12** | `a_registration_on_a_shut_down_client_is_refused` | A registration on a client whose pool has already shut down fails `Disconnected` and never reaches the component, so a dead client refuses rather than registering something nothing drives | Implemented |
| **LIF-13** | `a_registration_the_caller_gives_up_on_is_unregistered` | A registration dropped while `on_register` is still running is unregistered once that call returns, surfacing `on_unregister` and leaving the service id free for a successor | Implemented |

### Deferred scenarios

| Scenario | Why it is deferred |
| :--- | :--- |
| **Silent transport death** | Distinguishing silent peer death, keepalive timing, and half-open connections from graceful closures needs a transport that can go quiet without closing. The proxy can kill a connection outright (LIF-08) but cannot leave it silently half-open, which is what keepalive detection needs |
| **CLA vanished-client mid-`Forward` residue** | Abandonment and dropped-session teardown are pinned in-crate (CLA-03, CLA-16); a client vanishing mid-`Forward` drive, with the rendezvous claimed and chunks in flight, should be pinned end to end once a killable transport reaches the CLA suite |
| **Many small calls on one connection** | The h2 small-frame budget failure found in review needs a run of at least 200 short calls on a single channel to reproduce, which no scenario here issues |
