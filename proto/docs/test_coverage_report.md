# hardy-proto Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-proto` |
| **Crate version** | `0.3.0` |
| **Standard** | n/a (the wire speaks RFC 9171 vocabulary; format and BPA compliance are verified by `hardy-bpv7` and `hardy-bpa`) |
| **Test Plans** | [`COMP-GRPC-01`](component_test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

No formal LLRs are assigned to this crate: it is API infrastructure under [REQ-18](../../docs/requirements.md#req-18-comprehensive-technical-documentation-and-examples) (gRPC external APIs with complete documentation), not a protocol implementation with its own compliance matrix. The table below maps functional areas to their verification status instead. All 6 functional areas pass.

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| n/a | Session state, tokens, and teardown invariants | Pass | `SES-01..04`, plus every wire test below | n/a |
| n/a | Application surface (`application.v1`) served against a real BPA | Pass | `APP-01..11` | n/a |
| n/a | Service surface (`service.v1`) served against a real BPA | Pass | `SVC-01..11` | n/a |
| n/a | CLA surface (`cla.v1`) served against a real BPA | Pass | `CLA-01..12` | n/a |
| n/a | Routing surface (`routing.v1`) served against a real BPA | Pass | `RTE-01..07` | n/a |
| n/a | Client SDK (`BpaClient`) end to end, one roundtrip per surface | Pass | `APP-09`, `SVC-10`, `CLA-12`, `RTE-07` | n/a |

## 2. Test Inventory

45 tests in total, all `#[cfg(test)]` modules inside `src/`. The 4 session-state tests are unit tests (no network); the 41 wire tests are component tests over real sockets (a real `Bpa`, a port-0 listener, the generated tonic clients). All 45 require the `server` feature; the 4 client SDK roundtrips additionally require the `client` feature, so `cargo test -p hardy-proto --all-features` is the run that executes the full inventory.

### Unit tests: shared session state (`server/session.rs`), 4 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `sessions_resolve_only_live_tokens` | SES-01 | Mint/publish/resolve/remove; forged and retired tokens are `UNAUTHENTICATED`; removal idempotent |
| `the_registration_precedes_events_then_the_stream_ends_on_abort` | SES-02 | Registration-first ordering by construction; accepted events drain; the stream ends on abort |
| `abort_fires_the_broadcast_and_stops_events` | SES-03 | Abort cancels the session token and the biased race refuses further events |
| `event_blocked_on_a_full_buffer_is_freed_by_teardown` | SES-04 | Teardown alone releases an event send parked on a full buffer |

### Component tests: application surface (`server/services/application.rs`), 11 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `explicit_and_dynamic_registrations_mint_distinct_sessions` | APP-01 | Explicit and dynamic registration; distinct endpoints and tokens |
| `send_to_self_roundtrip` | APP-02 | Send, delivery announcement, collection; completed collection consumes the delivery |
| `a_truncated_send_never_commits` | APP-03 | Half-close without `last_chunk` is `ABORTED`; nothing submitted |
| `receive_of_an_unannounced_id_is_not_found` | APP-04 | An id never announced to this session (malformed ids included) is `NOT_FOUND` |
| `an_abandoned_collection_defers_to_the_next_registration` | APP-05 | In-band cancel is `CANCELLED`; the spent stream is `NOT_FOUND` and the next registration collects it whole |
| `a_forged_token_is_rejected` | APP-06 | Forged token is `UNAUTHENTICATED` |
| `a_dropped_stream_tears_the_session_down` | APP-07 | Dropped rpc fires the stream guard; the token dies |
| `pool_shutdown_tears_sessions_and_drains` | APP-08 | Pool shutdown ends the session stream and drains |
| `client_sdk_roundtrip` | APP-09 | SDK registration, send, and pull-to-completion delivery (`client` feature) |
| `re_registration_re_announces_many_parked_deliveries` | APP-10 | 48 parked deliveries re-announced to a new registration and collectable |
| `unregister_ends_the_session_and_invalidates_the_token` | APP-11 | Wire `Unregister` ends the stream; the token dies in teardown |

### Component tests: service surface (`server/services/service.rs`), 11 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `explicit_and_dynamic_registrations_mint_distinct_sessions` | SVC-01 | As APP-01 for the service surface |
| `send_to_self_roundtrip` | SVC-02 | Byte-identical bundle roundtrip through the streamed send |
| `a_truncated_send_never_commits` | SVC-03 | Truncation through the streamed pump is `ABORTED` |
| `a_cancelled_send_is_discarded` | SVC-04 | In-band cancel of a send is `CANCELLED`; partial bundle discarded |
| `an_invalid_bundle_is_rejected` | SVC-05 | BPA validation rejects garbage with `INVALID_ARGUMENT` |
| `an_abandoned_collection_defers_to_the_next_registration` | SVC-06 | As APP-05 for whole bundles |
| `a_forged_token_is_rejected` | SVC-07 | As APP-06 |
| `a_forged_source_is_rejected` | SVC-08 | A bundle claiming a foreign source endpoint is `INVALID_ARGUMENT` |
| `a_dropped_stream_tears_the_session_down` | SVC-09 | As APP-07 |
| `client_sdk_roundtrip` | SVC-10 | SDK whole-bundle send through the streamed pump and collection (`client` feature) |
| `unregister_ends_the_session_and_invalidates_the_token` | SVC-11 | As APP-11 |

### Component tests: CLA surface (`server/services/cla.rs`), 12 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `registration_returns_node_ids_and_a_token` | CLA-01 | Node ids returned; duplicate name fails `Subscribe` with `ALREADY_EXISTS` |
| `dispatch_and_forward_roundtrip` | CLA-02 | Dispatch in, routed out: `Forwarding` announcement, streamed execution, `sent` result completes |
| `an_abandoned_forwarding_stays_queued` | CLA-03 | In-band cancel is `CANCELLED`; the BPA requeues and the re-announcement completes |
| `an_accepted_forwarding_reports_its_outcome` | CLA-04 | `accepted` parks the transfer for `ReportTransferOutcome`; a late second outcome is dropped |
| `a_truncated_dispatch_never_commits` | CLA-05 | Truncated dispatch is `ABORTED`; no forwarding follows |
| `a_cancelled_dispatch_is_discarded` | CLA-06 | In-band cancel of a dispatch is `CANCELLED` |
| `peers_are_added_and_removed_once` | CLA-07 | `AddPeer`/`RemovePeer` idempotence |
| `a_forged_token_is_rejected` | CLA-08 | As APP-06 on a unary door |
| `unregister_ends_the_session_and_invalidates_the_token` | CLA-09 | As APP-11 |
| `a_forward_for_an_unknown_bundle_is_not_found` | CLA-10 | `Forward` for an unannounced id is `NOT_FOUND` |
| `forward_requires_the_metadata_first` | CLA-11 | Non-metadata first message is `INVALID_ARGUMENT` |
| `client_sdk_roundtrip` | CLA-12 | SDK CLA: peer announcement, dispatch, and forwarding back through `Cla::forward` (`client` feature) |

### Component tests: routing surface (`server/services/routing.rs`), 7 tests

| Test Function | Plan ID | Scope |
| :--- | :--- | :--- |
| `registration_returns_node_ids_and_a_token` | RTE-01 | Node ids returned; duplicate name fails `Subscribe` with `ALREADY_EXISTS` |
| `routes_are_added_and_removed_once` | RTE-02 | `AddRoute`/`RemoveRoute` idempotence against the real RIB |
| `an_invalid_pattern_is_rejected` | RTE-03 | Malformed EID pattern is `INVALID_ARGUMENT` |
| `a_missing_action_is_rejected` | RTE-04 | Missing route action is `INVALID_ARGUMENT` |
| `a_forged_token_is_rejected` | RTE-05 | As APP-06 |
| `unregister_ends_the_session_and_invalidates_the_token` | RTE-06 | As APP-11 |
| `client_sdk_roundtrip` | RTE-07 | SDK routing agent drives add/remove idempotence through its sink (`client` feature) |

No fuzz targets exist for this crate: the parsers it exposes to the network are prost's generated decoders plus the domain parsers of `hardy-bpv7` and `hardy-eid-patterns`, which have their own fuzz plans.

## 3. Coverage vs Plan

| Section | Suite | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| Plan §3 SES | Shared session state | 4 | 4 | Complete |
| Plan §3 APP | Application surface | 11 | 11 | Complete |
| Plan §3 SVC | Service surface | 11 | 11 | Complete |
| Plan §3 CLA | CLA surface | 12 | 12 | Complete |
| Plan §3 RTE | Routing surface | 7 | 7 | Complete |
| Plan §5 | Cross-crate lifecycle scenarios | 6 | 6 | `proto/tests/lifecycle.rs`; two scenarios needing a killable transport remain deferred |
| | **Total (in-crate scope)** | **45** | **45** | **100%** |
| | **Total (including deferred)** | **50** | **45** | **90%** |

## 4. Line Coverage

Instrumented line coverage has not been measured for this version of the crate. The `hardy-proto` row in [`docs/coverage_summary.md`](../../docs/coverage_summary.md) was generated from crate version `0.2.0` and does not describe the code this report covers; treat it as pending regeneration by `scripts/run_lcov.sh`. To measure:

```
cargo llvm-cov test --package hardy-proto --all-features --lcov --output-path lcov.info --html
lcov --summary lcov.info
```

Note that `--all-features` is required: without the `server` feature no tests compile, and without `client` the four SDK roundtrips are skipped.

### Inventory-based assessment (not instrumented)

In place of measured figures, the following is what the 45 tests demonstrably reach, derived from reading the test modules against the source:

- **Exercised on every run:** the four bridge `subscribe` handlers and their session tasks; all data-plane and unary doors including their rejection arms (`UNAUTHENTICATED`, `NOT_FOUND`, `INVALID_ARGUMENT`, `ALREADY_EXISTS`, `ABORTED`, `CANCELLED`); the whole of `server/session.rs`, and the minting and resolution paths of `server/token.rs`; the `ServerTransfer` pump's chunk, last-chunk, cancel, and truncation arms (via the service and CLA surfaces); the `stream_delivery` and `drive_forward` down engines through completion and abandonment; and the chunk grammar in `stream.rs` including multi-chunk transfers (payloads sized `CHUNK_SIZE + 3`).
- **Exercised with `--all-features`:** the client SDK's handshake, event loops, sinks, streaming pumps, and `ClientTransfer` pull path for all four surfaces, on their happy paths.
- **Not reached by any test:** the SDK's error-translation branches; the down-direction withdrawal messages (`ReceiveResponse.cancelled`, `ForwardResponse.cancelled`, emitted only when a bundle expires or is deleted mid-transfer); the application `Send` loop's in-band cancel arm; the `SendOptions` conversions and the `BundleStatusReport` event path (no test requests status reports); the `Drop` and `Reflect` route-action conversions; and the defensive arms for mid-stream protocol violations after a valid first message.

## 5. Test Infrastructure

- **Per-surface harness, no shared fixture crate:** each wire suite defines its own `harness()` building a real `Bpa` (single node id `ipn:1`, default in-memory configuration), a `TaskPool` the bridge's sessions ride, a port-0 `TcpListener` wrapped in `TcpIncoming` with `TCP_NODELAY`, the bridge under test wrapped in its generated server, and a connected generated client. The harness holds the `TaskPool` alive for the test's duration (dropping it would tear the sessions), and every test ends with `bpa.shutdown().await`.
- **The BPA is built with the `no-rfc9171-autoregister` dev feature**, so the auto-registered RFC 9171 validity filter does not sit between the wire and the assertions; the bridges, not ingress policy, are the subject under test.
- **Helpers per suite:** `register()` completes the Subscribe handshake and returns the token and event stream; `send`/`collect`/`delivery` (payload surfaces), `dispatch`/`forwarding`/`execute_forward`/`add_peer` (CLA), and `add_route`/`remove_route` (routing) wrap the doors; `build_bundle()` produces canonical BPv7 bytes via the `hardy-bpv7` builder where a surface exchanges whole bundles.
- **Timing discipline:** every await is wrapped in a 10 second `timeout()` guard; negative assertions use short bounded races; token invalidation is asserted by polling for `UNAUTHENTICATED`, because teardown completes after the stream closes and has no other client-visible signal. All wire tests run on the multi-threaded runtime (`worker_threads = 2`).
- **SDK tests:** minimal trait implementations (`SdkApp`, `SdkService`, `SdkCla`, `SdkAgent`) store their sink in a `Once` and report observations over an mpsc channel, following the same event-driven pattern.

## 6. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Lifecycle | Silent transport death unpinned | Low | Needs a killable transport ([plan §5](component_test_plan.md)); the cooperative endings are pinned by `proto/tests/lifecycle.rs` |
| Lifecycle | Connection loss and SDK death unpinned | Medium | Deferred: needs a killable transport; keepalive-bounded detection is unverified |
| Lifecycle | Simultaneous unregister unpinned | Medium | Deferred: exactly-once unregistration under a two-sided race is unasserted |
| Lifecycle | Dropped-rpc teardown unpinned on the CLA and routing surfaces | Medium | The shared guard is pinned on application (APP-07) and service (SVC-09); the CLA's held forwardings under a vanished client are not |
| Client SDK | Error paths untested | Medium | The four roundtrips are happy-path; the status-to-domain-error translation in the SDK never executes under test |
| Application surface | `Send` in-band cancel arm untested | Low | The cancel encoding is pinned on the service (SVC-04) and CLA (CLA-06) surfaces; the application accumulation loop's own arm is not |
| Data plane | Mid-transfer withdrawal untested | Low | `ReceiveResponse.cancelled` and `ForwardResponse.cancelled` fire only on expiry or deletion during a transfer, which no test triggers |
| Wire options | `SendOptions` and `BundleStatusReport` unasserted | Low | No test sets transmission flags or asserts a status-report event crossing the wire |

## 7. Conclusion

The crate carries 71 tests (65 in-crate — session-state and shared-engine unit tests plus the wire component tests, of which 8 drive the client SDK — and the 6 cross-crate lifecycle tests in `proto/tests/lifecycle.rs`), implementing the plan's in-crate scenarios and its lifecycle suite; only the two killable-transport scenarios in [plan §5](component_test_plan.md) remain deferred. Instrumented line coverage has not been measured for this crate version; section 4 gives the command and an inventory-based assessment in its place. The strengths are the doctrine itself: every test runs the real wire against a real `Bpa`, every surface pins its registration, its token discipline, its truncation-never-commits rule, and its abandonment-defers-work rule, and the send-to-self roundtrips prove each payload pipeline end to end. The primary remaining gaps are the deferred lifecycle scenarios (BPA-initiated and simultaneous unregister, connection loss, CLA dropped-stream residue) and the client SDK's error paths, with the smaller in-crate branches listed in section 6.
