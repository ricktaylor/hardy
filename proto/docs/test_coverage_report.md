# hardy-proto Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-proto` |
| **Crate version** | `0.3.0` |
| **Standard** | n/a (the wire speaks RFC 9171 vocabulary; format and BPA compliance are verified by `hardy-bpv7` and `hardy-bpa`) |
| **Test Plans** | [`COMP-GRPC-01`](component_test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

No formal LLRs are assigned to this crate: it is API infrastructure under [REQ-18](../../docs/requirements.md#req-18-comprehensive-technical-documentation-and-examples) (gRPC external APIs with complete documentation), not a protocol implementation with its own compliance matrix. The table below maps functional areas to their verification status instead. All 7 functional areas pass what they execute, but three of them carry `#[ignore]`d tests: five scenarios are written and green only against BPA behaviour that is still in review, so they are not verified today. [`TODO.md`](TODO.md) names the BPA change each one waits on.

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| n/a | Session state, tokens, and teardown invariants | Pass | `SES-02..04` (SES-01 is pinned at the wire), plus every wire test below | n/a |
| n/a | Shared announce-and-collect table (delivery and forward rendezvous) | Pass | `REG-01..03` | n/a |
| n/a | Application surface (`hardy.application.v1`) served against a real BPA | Pass, `APP-24` deferred | `APP-01..27` | n/a |
| n/a | Service surface (`hardy.service.v1`) served against a real BPA | Pass | `SVC-01..12` | n/a |
| n/a | CLA surface (`hardy.cla.v1`) served against a real BPA | Pass, `CLA-03`, `CLA-05` and `CLA-06` deferred | `CLA-01..18` | n/a |
| n/a | Routing surface (`hardy.routing.v1`) served against a real BPA | Pass | `RTE-01..09` | n/a |
| n/a | Client SDK (`BpaClient`) end to end | Pass, `LIF-08` deferred | `APP-21..23`, `APP-26`, `SVC-10..11`, `CLA-15..17`, `RTE-08`, plus the lifecycle suite | n/a |

## 2. Test Inventory

88 tests in total: 78 in-crate `#[cfg(test)]` tests under `src/`, and 10 cross-crate lifecycle tests in `proto/tests/lifecycle.rs`. The 17 unit tests (3 session-state, 3 announce-table, 3 token, 6 chunk-grammar, 1 timestamp, 1 early-result policy) run no network; the 61 wire tests are component tests over real sockets (a real `Bpa`, a port-0 listener, the generated tonic clients). Ten in-crate tests additionally require the `client` feature (the SDK roundtrips, the reply-from-within-a-delivery pin, the SDK decline-redelivery and deferred-outcome paths, and the delivery-report round-trips), as do all 10 lifecycle tests, so `cargo test -p hardy-proto --all-features` is the run that executes the full inventory.

Five of those 87 are `#[ignore]`d: APP-24, CLA-03, CLA-05, CLA-06 and LIF-08. Each asserts a promise the v1 wire makes and the BPA does not yet keep, so a full-feature run today is 82 executed, 5 ignored. [`TODO.md`](TODO.md) names the BPA change that un-ignores each one.

### Unit tests: shared session state (`server/session.rs`), 3 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `the_registration_precedes_events_then_the_stream_ends_on_abort` | SES-02 | Registration-first ordering by construction; accepted events drain; the stream ends on abort |
| `abort_fires_the_broadcast_and_stops_events` | SES-03 | Abort cancels the session token and the biased race refuses further events |
| `event_blocked_on_a_full_buffer_is_freed_by_teardown` | SES-04 | Teardown alone releases an event send parked on a full buffer |

### Unit tests: shared announce-and-collect table (`server/announce.rs`), 3 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `a_collect_is_single_use` | REG-01 | An announced entry is collectable exactly once; a second collect misses |
| `a_dropped_announcement_is_withdrawn` | REG-02 | An abandoned announcement leaves no entry behind |
| `a_stale_announcement_spares_a_successor` | REG-03 | A superseded announcement withdraws only its own entry, leaving the successor's collectable |

### Unit tests: the chunk grammar (`transfer.rs`), 6 tests

Direct tests of the re-framing every outgoing transfer goes through, whose edge cases the wire suites only reach by inference. No plan IDs: this is the grammar the plan's transfer scenarios presuppose, not a scenario of its own.

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `an_empty_intermediate_segment_yields_nothing` | n/a | An empty `Segment::Next` puts no chunk on the wire |
| `an_empty_final_segment_still_ends_the_transfer` | n/a | An empty `Segment::Final` still yields its empty last chunk |
| `a_segment_within_the_bound_is_one_chunk` | n/a | Under `CHUNK_SIZE`, one chunk, final iff the segment was |
| `a_segment_of_exactly_the_bound_is_one_chunk` | n/a | At exactly `CHUNK_SIZE`, one chunk and no empty trailer |
| `only_the_last_chunk_of_a_final_segment_is_final` | n/a | A split final segment marks only its last chunk `Final`, and the split preserves the bytes |
| `no_chunk_of_an_intermediate_segment_ends_the_transfer` | n/a | A split intermediate segment yields only `Next` chunks, bytes preserved |

### Component tests: application surface (`server/services/application.rs`), 26 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `explicit_and_dynamic_registrations_mint_distinct_sessions` | APP-01 | Explicit and dynamic registration; distinct endpoints and tokens |
| `send_to_self_roundtrip` | APP-02 | Send, delivery announcement, collection; completed collection consumes the delivery |
| `a_truncated_send_never_commits` | APP-03 | Half-close without `last_chunk` is `ABORTED`; nothing submitted |
| `a_cancelled_send_is_discarded` | APP-04 | In-band cancel of the `Send` accumulation loop is `CANCELLED`; the partial ADU is discarded |
| `receive_of_an_unannounced_id_is_not_found` | APP-05 | An id never announced to this session (malformed ids included) is `NOT_FOUND` |
| `an_abandoned_collection_defers_to_the_next_registration` | APP-06 | In-band cancel is `CANCELLED`; the spent stream is `NOT_FOUND` and the next registration collects it whole |
| `a_forged_token_is_rejected` | APP-07 | Forged token is `UNAUTHENTICATED` |
| `a_dropped_stream_tears_the_session_down` | APP-08 | Dropped rpc fires the stream guard; the token dies |
| `pool_shutdown_tears_sessions_and_drains` | APP-09 | Pool shutdown ends the session stream and drains |
| `an_empty_adu_delivers_end_to_end` | APP-10 | An empty ADU delivers as a lone empty `last_chunk` completion, never a truncation |
| `a_declared_adu_size_above_the_bound_is_rejected_preflight` | APP-11 | Above-bound declared size is rejected pre-flight; within the bound the declaration is a hint and an inaccurate one still commits on `last_chunk` |
| `a_receive_racing_the_announcement_lands` | APP-12 | A `Receive` racing the announcement lands once the entry exists; an early `NOT_FOUND` neither consumes nor poisons |
| `session_death_mid_receive_defers_the_delivery` | APP-13 | Session death with the final segment unpulled leaves the bundle parked; the next registration collects it whole |
| `a_cancel_after_the_last_chunk_parks_the_delivery` | APP-14 | Completion is the ack, not the last chunk: a cancel after the final chunk with no ack re-announces the bundle |
| `a_full_receipt_without_an_ack_parks_the_delivery` | APP-15 | A full receipt followed by silence commits nothing; the bundle is re-announced to the next registration |
| `an_ack_before_the_final_chunk_never_commits` | APP-16 | An ack before the last chunk is a protocol violation, never a commit; the collection ends without its last chunk and the bundle is re-announced |
| `pool_shutdown_survives_a_claimed_unread_receive` | APP-17 | A claimed, unread collection does not wedge pool shutdown; the parked writer abandons its terminal status |
| `a_stalled_session_does_not_starve_other_registrations` | APP-18 | A stalled session parks its own announcements past its event buffer while other registrations keep delivering |
| `a_dtn_registration_needs_a_dtn_node_id` | APP-19 | A `dtn`-scheme registration on an ipn-only node fails the handshake with `FAILED_PRECONDITION` |
| `a_dtn_registration_binds_the_dtn_endpoint` | APP-20 | A `dtn`-scheme registration binds the `dtn` endpoint on a node that declares a `dtn` node id |
| `client_sdk_roundtrip` | APP-21 | SDK registration, send, and pull-to-completion delivery (`client` feature) |
| `an_sdk_decline_after_full_receipt_is_redelivered` | APP-22 | An SDK app returning `Err` from `on_deliver` after buffering the whole ADU sends no ack; the bundle stays parked and the next registration receives it (`client` feature) |
| `a_delivery_report_reaches_the_sending_application` | APP-23 | A requested delivery report round-trips: collection generates it, the BPA consumes it at its admin endpoint, and the sender is notified over the wire (`client` feature) |
| `re_registration_re_announces_many_parked_deliveries` | APP-24 | 48 parked deliveries re-announced to a new registration and collectable. `#[ignore]`d: needs per-delivery concurrency in the BPA |
| `unregister_ends_the_session_and_invalidates_the_token` | APP-25 | Wire `Unregister` ends the stream; the token dies in teardown (asserted on the `torn_down` barrier) |
| `an_sdk_reply_from_within_a_delivery_does_not_deadlock` | APP-26 | An SDK app that replies through its own sink from inside `on_deliver` does not self-deadlock, even with more echoes in flight than the concurrent-delivery bound (`client` feature) |
| `an_rpc_abandoned_during_registration_ends_unregistered` | APP-27 | An rpc abandoned while the BPA is committing the registration still ends unregistered: the commit completes on the workflow's own task and is then undone |

### Component tests: service surface (`server/services/service.rs`), 12 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `explicit_and_dynamic_registrations_mint_distinct_sessions` | SVC-01 | As APP-01 for the service surface |
| `send_to_self_roundtrip` | SVC-02 | Byte-identical bundle roundtrip through the streamed send |
| `a_truncated_send_never_commits` | SVC-03 | Truncation through the streamed writer is `ABORTED` |
| `a_cancelled_send_is_discarded` | SVC-04 | In-band cancel of a send is `CANCELLED`; partial bundle discarded |
| `an_invalid_bundle_is_rejected` | SVC-05 | BPA validation rejects garbage with `INVALID_ARGUMENT` |
| `an_abandoned_collection_defers_to_the_next_registration` | SVC-06 | As APP-06 for whole bundles |
| `a_forged_token_is_rejected` | SVC-07 | As APP-07 |
| `a_forged_source_is_rejected` | SVC-08 | A bundle claiming a foreign source endpoint is `INVALID_ARGUMENT` |
| `a_dropped_stream_tears_the_session_down` | SVC-09 | As APP-08 |
| `client_sdk_roundtrip` | SVC-10 | SDK whole-bundle send through the streamed writer and collection (`client` feature) |
| `a_delivery_report_reaches_the_sending_service` | SVC-11 | A service-built bundle requesting a delivery report gets it back through the wire: the BPA consumes the report at its admin endpoint and the origin registration is notified (`client` feature) |
| `unregister_ends_the_session_and_invalidates_the_token` | SVC-12 | As APP-25 |

### Component tests: CLA surface (`server/services/cla.rs`), 18 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `registration_returns_node_ids_and_a_token` | CLA-01 | Node ids returned; duplicate name fails `Subscribe` with `ALREADY_EXISTS` |
| `dispatch_and_forward_roundtrip` | CLA-02 | Dispatch in (answered `ACCEPTANCE_ACCEPTED`), routed out: `Forwarding` announcement, streamed execution, `sent` result completes |
| `an_abandoned_forwarding_stays_queued` | CLA-03 | In-band cancel is `CANCELLED`; the BPA requeues and the re-announcement completes. `#[ignore]`d: needs the BPA to retry a synchronous `Cla::forward` failure by its kind |
| `an_accepted_forwarding_reports_its_outcome` | CLA-04 | `accepted` parks the transfer for `ReportTransferOutcome`; a late second outcome is dropped |
| `a_truncated_dispatch_never_commits` | CLA-05 | Truncated dispatch is `ABORTED`; no forwarding follows. `#[ignore]`d: needs the BPA to commit a streamed ingress bundle only on its final segment |
| `a_cancelled_dispatch_is_discarded` | CLA-06 | In-band cancel of a dispatch is `CANCELLED`. `#[ignore]`d: the same final-segment gate |
| `peers_are_added_and_removed_once` | CLA-07 | `AddPeer`/`RemovePeer` idempotence |
| `a_forged_token_is_rejected` | CLA-08 | As APP-07 on a unary door |
| `a_dropped_stream_tears_the_session_down` | CLA-09 | Dropped rpc fires the stream guard on the CLA surface; the token dies |
| `unregister_ends_the_session_and_invalidates_the_token` | CLA-10 | As APP-25 |
| `a_forward_for_an_unknown_bundle_is_not_found` | CLA-11 | `Forward` for an unannounced id is `NOT_FOUND` |
| `forward_requires_the_metadata_first` | CLA-12 | Non-metadata first message is `INVALID_ARGUMENT` |
| `a_duplicate_forward_for_a_live_call_is_not_found` | CLA-13 | The `Forward` claim is single-executor: a duplicate `Forward` for an id a live call already holds is `NOT_FOUND`, and the live call completes untouched |
| `lane_count_is_validated_at_registration` | CLA-14 | Zero and above-bound declared lane counts are `INVALID_ARGUMENT`; the bound itself registers |
| `an_sdk_deferred_outcome_completes_the_transfer` | CLA-15 | An SDK CLA returning a deferred forwarding outcome later completes the transfer through `ReportTransferOutcome` (`client` feature) |
| `the_sdk_rejects_an_over_declared_lane_count` | CLA-16 | The SDK surfaces the registration-time rejection of an over-declared lane count (`client` feature) |
| `client_sdk_roundtrip` | CLA-17 | SDK CLA: peer announcement, dispatch, and forwarding back through `Cla::forward` (`client` feature) |
| `an_early_result_admits_only_no_neighbour` | CLA-18 | The policy for a result that arrives before the last chunk: `no_neighbour` is honoured, `sent` and `accepted` are `INVALID_ARGUMENT`, and an abandonment keeps its own status |

### Component tests: routing surface (`server/services/routing.rs`), 8 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `registration_returns_node_ids_and_a_token` | RTE-01 | Node ids returned; duplicate name fails `Subscribe` with `ALREADY_EXISTS` |
| `routes_are_added_and_removed_once` | RTE-02 | `AddRoute`/`RemoveRoute` idempotence against the real RIB |
| `an_invalid_pattern_is_rejected` | RTE-03 | Malformed EID pattern is `INVALID_ARGUMENT` |
| `a_missing_action_is_rejected` | RTE-04 | Missing route action is `INVALID_ARGUMENT` |
| `a_forged_token_is_rejected` | RTE-05 | As APP-07 |
| `a_dropped_stream_tears_the_session_down` | RTE-06 | Dropped rpc fires the stream guard on the routing surface; the token dies |
| `unregister_ends_the_session_and_invalidates_the_token` | RTE-07 | As APP-25 |
| `client_sdk_roundtrip` | RTE-08 | SDK routing agent drives add/remove idempotence through its sink, and the sink refuses the reserved drop reason before the wire (`client` feature) |
| `a_reserved_drop_reason_is_rejected` | RTE-09 | RFC 9171's reserved reason code 255 in a `drop` action is `INVALID_ARGUMENT`; an unassigned code is carried through |

### Cross-crate lifecycle tests (`proto/tests/lifecycle.rs`), 10 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `a_client_unregister_round_trips` | LIF-01 | A client `Unregister` ends the session, the SDK surfaces `on_unregister`, the registration handle resolves `Ok`, and the service id frees for a successor |
| `bpa_initiated_teardown_reaches_the_client` | LIF-02 | Shutting the BPA down unregisters the server's component, ends the wire session, the SDK surfaces `on_unregister`, and the handle resolves `Err(Disconnected)` |
| `connection_loss_defers_announced_bundles` | LIF-03 | A dead client's parked, uncollected bundle is re-announced to the endpoint's next registration, which collects it whole |
| `simultaneous_unregister_settles` | LIF-04 | Unregistration from both ends settles with neither side hanging and exactly one observed `on_unregister` |
| `dropping_the_sink_unregisters` | LIF-05 | A never-stored sink half-closes the session; the server unregisters, the SDK surfaces `on_unregister`, and the handle resolves `Ok` because the drop was this side asking |
| `a_server_restart_disconnects_the_client` | LIF-06 | A server teardown (a restart from the client's view) surfaces `on_unregister` and resolves the handle `Err(Disconnected)`, there being no local unregister behind it; the orphaned sink fails rather than blocking, its token dead |
| `shutdown_interrupts_a_stuck_delivery` | LIF-07 | An `on_deliver` that never returns is abandoned on client pool shutdown; the session still runs its unregistration to completion |
| `deliveries_collect_concurrently` | LIF-08 | Two announced bundles are inside `on_deliver` at once, so a slow collection does not stall the next announcement. `#[ignore]`d: needs per-delivery concurrency in the BPA |
| `a_transport_loss_surfaces_the_session_error` | LIF-09 | A connection killed without trailers ends the session with the transport's own error carried whole: the handle yields the actual status, code and source chain intact, rather than the bare `Disconnected` of an orderly close |
| `a_dead_session_fails_a_send_from_a_stalled_producer` | LIF-10 | A streamed request answered before its request stream is read returns that answer however slow the local producer: a dead session's send fails as `Disconnected` with a producer that never yields a segment. Pins `client::write_transfer`, shared by the application and service `Send` and the CLA `Dispatch` |

No fuzz targets exist for this crate: the parsers it exposes to the network are prost's generated decoders plus the domain parsers of `hardy-bpv7` and `hardy-eid-patterns`, which have their own fuzz plans.

## 3. Coverage vs Plan

| Section | Suite | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| Plan §3 SES | Shared session state | 4 | 4 | Complete |
| Plan §3 REG | Shared announce-and-collect table | 3 | 3 | Complete |
| Plan §3 APP | Application surface | 27 | 27 | Complete; APP-24 `#[ignore]`d |
| Plan §3 SVC | Service surface | 12 | 12 | Complete |
| Plan §3 CLA | CLA surface | 18 | 18 | Complete; CLA-03, CLA-05 and CLA-06 `#[ignore]`d |
| Plan §3 RTE | Routing surface | 9 | 9 | Complete |
| Plan §5 | Cross-crate lifecycle scenarios | 10 | 10 | `proto/tests/lifecycle.rs`; LIF-08 `#[ignore]`d, and the silent-transport-death and mid-`Forward` vanish scenarios remain deferred |
| | **Total (in-crate scope)** | **73** | **73** | **100%** |
| | **Total (including deferred)** | **85** | **83** | **98%** |

## 4. Line Coverage

Instrumented line coverage has not been measured for this version of the crate. The `hardy-proto` row in [`docs/coverage_summary.md`](../../docs/coverage_summary.md) was generated from crate version `0.2.0` and does not describe the code this report covers; treat it as pending regeneration by `scripts/run_lcov.sh`. To measure:

```
cargo llvm-cov test --package hardy-proto --all-features --lcov --output-path lcov.info --html
lcov --summary lcov.info
```

Note that `--all-features` is required: without the `server` feature no tests compile, and without `client` the ten SDK-driven in-crate tests and all ten lifecycle tests are skipped.

### Inventory-based assessment (not instrumented)

In place of measured figures, the following is what the 81 tests demonstrably reach, derived from reading the test modules against the source:

- **Exercised on every run:** the four surfaces' `subscribe` handlers and the shared workflow their session tasks run; all data-plane and unary doors including their rejection arms (`UNAUTHENTICATED`, `NOT_FOUND`, `INVALID_ARGUMENT`, `ALREADY_EXISTS`, `FAILED_PRECONDITION`, `ABORTED`, `CANCELLED`); the whole of `server/session.rs` and the shared announce-and-collect table in `server/announce.rs`; the minting and resolution paths of `token.rs`; the ack-gated delivery commit protocol in every arm (completion commits, a full receipt without an ack parks, an ack before the final chunk is refused, a mid-stream session death or post-last-chunk cancel re-announces); the server `adapter::RequestReader`'s chunk, last-chunk, cancel, and truncation arms; the shared response writer (`adapter::ResponseWriter`) on both the delivery and the `Forward` direction, through completion and abandonment; the ingress doors' in-band cancel arm (APP-04); the delivery-report event path end to end (APP-23, SVC-11); the empty-ADU and declared-size-preflight arms; the `dtn`-scheme registration precondition; and the chunk grammar in `transfer.rs`, directly in its own unit tests and through multi-chunk transfers.
- **Exercised with `--all-features`:** the client SDK's handshake, event loops, sinks, streaming writers, and client `adapter::ResponseReader` pull path for all four surfaces; the SDK's decline-then-redelivery path (APP-22), a reply issued through the sink from inside a pulled delivery (APP-26), its deferred forwarding-outcome path (CLA-15), and two registration-time rejections it surfaces (CLA-16, and the lifecycle suite's disconnection paths).
- **Not reached by any test:** the SDK's exhaustive status-to-domain error translation (only the decline, over-declared lane count, and disconnection arms execute); the down-direction withdrawal messages (`ReceiveResponse.cancelled`, `ForwardResponse.cancelled`, emitted only when a bundle expires or is deleted mid-transfer); the `Drop` and `Reflect` route-action conversions (including the reserved-reason-code rejection); and the transmission-flag `SendOptions` conversions beyond the delivery-report flag.

## 5. Test Infrastructure

- **The wire suites are inline `#[cfg(test)]` modules, not files under `tests/`:** every one of them asserts against the session teardown barrier (`Sessions::torn_down`), a `#[cfg(test)]`-only signal that is invisible from an integration test, and the alternative is the timing margin the test style guide forbids. The tests that need no such hook (the SDK-level lifecycle suite) live in `tests/lifecycle.rs`, where they drive the crate's public surface only.
- **Per-surface harness, no shared fixture crate:** each wire suite builds a real `Bpa` (single node id `ipn:1`, default in-memory configuration; `status_reports` enabled only for the surfaces whose report round-trip tests need it), a `TaskPool` the surface's sessions ride, a port-0 `TcpListener` wrapped in `TcpIncoming` with `TCP_NODELAY`, the surface under test wrapped in its generated server, and a connected generated client. The shared `serve` helper and fixtures (`build_bpa`, `ipn1`, `build_bundle`, `wait_torn_down`, `timeout`) live in an inline `#[cfg(test)] pub mod tests` in `server/services/mod.rs`, cross-imported by path. The harness holds the `TaskPool` alive for the test's duration (dropping it would tear the sessions), and every test ends with `bpa.shutdown().await`.
- **The BPA is built with the `no-rfc9171-autoregister` dev feature**, so the auto-registered RFC 9171 validity filter does not sit between the wire and the assertions; the server surfaces, not ingress policy, are the subject under test.
- **Helpers per suite:** `register()` completes the Subscribe handshake and returns the token and event stream; `send`/`collect`/`delivery` (payload surfaces), `dispatch`/`forwarding`/`execute_forward`/`add_peer` (CLA), and `add_route`/`remove_route` (routing) wrap the doors; `build_bundle()` produces canonical BPv7 bytes via the `hardy-bpv7` builder where a surface exchanges whole bundles.
- **Timing discipline:** every await is wrapped in a 10 second `timeout()` guard that only bounds a regression; negative assertions use short bounded races; token invalidation synchronizes on the `torn_down()` broadcast barrier the session machinery raises in teardown (`wait_torn_down`), then asserts the `UNAUTHENTICATED` rejection race-free, rather than polling. All wire tests run on the multi-threaded runtime (`worker_threads = 2`).
- **SDK tests:** minimal trait implementations (`SdkApp`, `SdkService`, `SdkCla`, `SdkAgent`) store their sink in a `Once` and report observations over an mpsc channel, following the same event-driven pattern.

## 6. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Lifecycle | Silent transport death unpinned | Medium | Needs a transport that can go quiet without closing (a proxy or a hard-killed process); the in-process suite kills connections outright (LIF-09) but cannot leave them silently half-open, so keepalive-bounded detection of a silently dead peer is unverified |
| Lifecycle | CLA vanished-client mid-`Forward` residue unpinned | Low | Abandonment and dropped-session teardown are pinned in-crate (CLA-03, CLA-09); a client vanishing mid-`Forward` drive (rendezvous claimed, chunks in flight) should be pinned end to end once a killable transport exists |
| Client SDK | Error-translation branches partially tested | Medium | The decline-redelivery, over-declared lane count, and disconnection arms execute; the exhaustive status-to-domain-error translation does not, and the carried-whole session ending is pinned only on the application surface (LIF-09): the identical `cla_session_error`/`routing_session_error` paths are unexercised |
| Data plane | Mid-transfer withdrawal untested | Low | `ReceiveResponse.cancelled` and `ForwardResponse.cancelled` fire only on expiry or deletion during a transfer, which no test triggers |
| Wire options | `Drop`/`Reflect` route-action conversions and non-report `SendOptions` flags unasserted | Low | The reserved-reason-code rejection and the transmission-flag conversions beyond the delivery-report flag have no test |

## 7. Conclusion

The crate carries 88 tests: 78 in-crate (17 unit tests covering the session state machine, the announce table, tokens, the chunk grammar, timestamps and the early-result policy, plus 61 wire component tests, of which 10 drive the client SDK) and 10 cross-crate lifecycle tests in `proto/tests/lifecycle.rs`. Every planned in-crate scenario and all but the two killable-transport lifecycle scenarios are implemented. Instrumented line coverage has not been measured for this crate version; section 4 gives the command and an inventory-based assessment in its place. The strengths are the doctrine itself: every wire test runs the real wire against a real `Bpa`, every surface pins its registration, its token discipline, its truncation-never-commits rule, and its abandonment-defers-work rule, and the ack-gated delivery commit protocol is pinned in every arm. The primary remaining gaps are the deferred killable-transport lifecycle scenarios and the client SDK's exhaustive error-translation paths, with the smaller in-crate branches listed in section 6.
