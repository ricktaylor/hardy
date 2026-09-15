# hardy-proto Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-proto` |
| **Crate version** | `0.3.0` |
| **Standard** | n/a (the wire speaks RFC 9171 vocabulary; format and BPA compliance are verified by `hardy-bpv7` and `hardy-bpa`) |
| **Test Plans** | [`COMP-GRPC-01`](component_test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

No formal LLRs are assigned to this crate. It is API infrastructure under [REQ-18](../../docs/requirements.md#req-18-comprehensive-technical-documentation-and-examples) (gRPC external APIs with complete documentation), not a protocol implementation with its own compliance matrix, so the table below maps functional areas to their verification status instead.

12 of 13 functional areas pass what they execute; the status mapping has no test of its own. Three areas carry an `#[ignore]`d test: those scenarios are written and assert a promise the v1 wire makes, but they are green only against BPA behaviour that is still in review, so they are not verified today. [`TODO.md`](TODO.md) names the BPA change each one waits on.

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| n/a | The chunked-transfer grammar, token minting and redaction, and timestamp conversion | Pass | Plan suite `UNT` | n/a |
| n/a | The stall defences: idle rule, rate floor, first-stall record, handshake bound | Pass | Plan suite `WDG` for the record, `RCV-01` to `RCV-06` for the idle rule and rate floor, plus the zero-limit wire tests `APP-16`, `APP-26`, `APP-27`, `APP-40`, `APP-41`, `CLA-11`, `SVC-03` | n/a |
| n/a | The announce-and-collect rendezvous, the bounded writer, and the bounded reader's size checks | Pass | Plan suite `RVZ`, plus `RCV-07` to `RCV-09` | n/a |
| n/a | Session event path, weak-sender retirement, and slot release | Pass | Plan suite `SES` | n/a |
| n/a | The server's bounds, and the per-session footprint the ceiling is read against | Pass | Plan suite `LIM`, plus `APP-32` and `APP-35` on the wire | n/a |
| n/a | Chunk-size negotiation, both ends, and the negotiated size as a bound on both directions | Pass | `UNT-01` to `UNT-04`, plus `APP-35` to `APP-39` on the wire | n/a |
| n/a | The opening of every call: handshake timeout, wrong first message, missing or malformed fields | Pass | `APP-40` to `APP-45`, `CLA-06` to `CLA-08`, `CLA-18`, `CLA-19`, `CLA-26`, `RTE-03`, `RTE-04` | n/a |
| n/a | The status a BPA refusal ends a call with (`server/status.rs`) | Not tested | Plan suite `STS`; the arms a wire scenario reaches are exercised only through it | n/a |
| n/a | Application API (`hardy.application.v1`) served against a real BPA | Pass, `APP-23` deferred | Plan suite `APP` | n/a |
| n/a | Service API (`hardy.service.v1`) served against a real BPA | Pass | Plan suite `SVC` | n/a |
| n/a | CLA API (`hardy.cla.v1`) served against a real BPA, including the deferred outcome | Pass, `CLA-03` deferred | Plan suite `CLA` | n/a |
| n/a | Routing API (`hardy.routing.v1`) served against a real BPA | Pass | Plan suite `RTE` | n/a |
| n/a | Client SDK (`BpaClient`) end to end | Pass, `LIF-10` deferred | Plan suite `LIF`, plus the eleven `client`-gated tests in the four wire suites | n/a |

## 2. Test Inventory

151 tests plus 5 doctests. The inventory is regenerated with `cargo test -p hardy-proto --all-features -- --list`; the per-test descriptions live in the [test plan](component_test_plan.md) and are not duplicated here.

| Binary | Tests | Plan suites | Kind |
| :--- | ---: | :--- | :--- |
| `--lib` (inline `#[cfg(test)]` modules) | 45 | `UNT` 15, `WDG` 4, `RCV` 9, `RVZ` 9, `SES` 6, `LIM` 2 | Unit, no network (`SES-05` and `SES-06` mount a server) |
| `tests/application.rs` | 46 | `APP` | Component, real wire |
| `tests/cla.rs` | 27 | `CLA` | Component, real wire |
| `tests/lifecycle.rs` | 11 | `LIF` | Component, real wire, SDK only |
| `tests/routing.rs` | 9 | `RTE` | Component, real wire |
| `tests/service.rs` | 13 | `SVC` | Component, real wire |
| **Total** | **151** | | |

Of these, 106 are integration tests under `proto/tests/` and 45 are inline `#[cfg(test)]` modules under `src/`: 10 in `chunking/mod.rs`, 9 in `chunking/receiver.rs`, 4 in `chunking/sender.rs`, 3 in `token.rs`, 1 in `timestamp.rs`, 1 in `lib.rs`, 4 in `server/watchdog.rs`, 5 in `server/announce.rs`, 2 in `server/limits.rs`, and 6 in `server/services/endpoint/application.rs`. Each inline module is there because it reads something crate-private: `watchdog` and `announce` are `pub(crate)` with no wire representation; `chunking`, `token` and `timestamp` are private modules; `chunking::receiver::BoundedChunkReceiver` is driven over scripted segment sources and read through its private `deadline()`; `chunking::sender::BoundedChunkSender` is driven directly with a held-back ending permit; `limits` derives the footprint from constants no client can see; the `lib.rs` test pins the redacting `Debug` that `build.rs` configures on the generated messages; and the six in `server/services/endpoint/application.rs` construct a `GrpcApplication` directly or read the `#[cfg(test)]` `opened` and `closed` hooks on `ApplicationServiceImpl`. Everything that can be asserted from outside the crate is.

The 5 doctests are the crate-level examples in `lib.rs` (one per feature), `server/mod.rs`, `server/limits.rs` and `client/bpa_client.rs`; all are `no_run`, so they verify that the documented code compiles against the public API.

Three tests are `#[ignore]`d: `re_registration_re_announces_many_parked_deliveries` (APP-23), `an_abandoned_forwarding_stays_queued` (CLA-03) and `deliveries_collect_concurrently` (LIF-10). A full-feature run is therefore 148 executed and 3 ignored, plus 5 doctests.

**What each feature set runs.** The `default` build is the wire contract alone: it runs 1 test (`UNT-15`, which needs only the generated types) and no doctests. `--features server` compiles 128 of the 151 (126 executed) plus 3 doctests: 44 in the library, 41 in `tests/application.rs`, 24 in `tests/cla.rs`, 8 in `tests/routing.rs`, 11 in `tests/service.rs`, and none in `tests/lifecycle.rs`; it leaves out the `client`-gated timestamp unit and the eleven SDK-driven tests in the four wire suites. `--all-features` is the run that executes the full inventory, and is what CI runs.

No fuzz targets exist for this crate. The parsers it exposes to the network are prost's generated decoders plus the domain parsers of `hardy-bpv7` and `hardy-eid-patterns`, which have their own fuzz plans.

## 3. Coverage vs Plan

| Section | Suite | Planned | Implemented | Status |
| :--- | :--- | ---: | ---: | :--- |
| Plan §3 UNT | Pure units | 15 | 15 | Complete |
| Plan §3 WDG | The watchdog | 4 | 4 | Complete |
| Plan §3 RCV | The bounded reader | 9 | 9 | Complete |
| Plan §3 RVZ | Rendezvous primitives | 9 | 9 | Complete |
| Plan §3 SES | Session event path | 6 | 6 | Complete |
| Plan §3 LIM | The server's bounds | 2 | 2 | Complete |
| Plan §3 STS | The status mapping | 1 | 0 | 1 remaining |
| Plan §3 APP | Application API | 46 | 46 | Complete; APP-23 `#[ignore]`d |
| Plan §3 SVC | Service API | 13 | 13 | Complete |
| Plan §3 CLA | CLA API | 27 | 27 | Complete; CLA-03 `#[ignore]`d |
| Plan §3 RTE | Routing API | 9 | 9 | Complete |
| Plan §5 LIF | SDK lifecycle | 11 | 11 | Complete; LIF-10 `#[ignore]`d |
| Plan §5 | Deferred scenarios | 3 | 0 | Need a killable or silenceable transport, or a long single-connection run |
| | **Total (implemented scope)** | **151** | **151** | **100%** |
| | **Total (including STS and deferred)** | **155** | **151** | **97%** |

## 4. Line Coverage

The `hardy-proto` row in [`docs/coverage_summary.md`](../../docs/coverage_summary.md) is the figure of record; it is generated by `scripts/run_lcov.sh` and corresponds to the crate version at the commit that includes it, so it lags the inventory above until the script is next run. To regenerate for this crate alone:

```
cargo llvm-cov test --package hardy-proto --all-features --lcov --output-path lcov.info --html
lcov --summary lcov.info
```

`--all-features` is required. Without `server` almost nothing compiles, and without `client` the lifecycle binary and 12 further tests do not run.

Two anomalies will show in any figure generated here. The generated prost and tonic code is compiled into the crate and is largely `Debug`, `Default` and encoder impls that no test touches, which depresses the line figure against hand-written code; and the `client` and `server` halves are near-disjoint, so a single-feature run reports the other half as entirely uncovered.

### What the executed tests reach

- **On every full-feature run:** the four APIs' `subscribe` handlers and the session task each one runs; every data-plane and unary door including its rejection arms (`UNAUTHENTICATED`, `NOT_FOUND`, `INVALID_ARGUMENT`, `ALREADY_EXISTS`, `FAILED_PRECONDITION`, `ABORTED`, `CANCELLED`, `RESOURCE_EXHAUSTED`, `DEADLINE_EXCEEDED`); the handshake bound on `Subscribe` and `Send`, and the first-message and required-field checks of every door; all five watchdog stages, each reached both in a unit suite and over the wire, and the stall a bounded transfer reports as a `TransferError` recorded on the watchdog through `Watchdog::stall`; the announce-and-collect table and the permit held back for the ending; the negotiated sizes, the `Registration` that carries them, and the negotiated chunk size enforced on a `Send` and applied to a `Receive`; a CLA's declared `max_bundle_size` echoed and enforced preflight; token minting, resolution, and redaction in `Debug`; the declared-size check in all three of its outcomes; the ack-gated commit in every arm; the forwarding commit in its `sent`, `accepted` then `completed`, and `accepted` then `failed` arms; the `ChunkReceiver`'s chunk, last-chunk, cancel and truncation arms, the `BoundedChunkReceiver`'s rate, chunk-size and declared-size checks, and the `BoundedChunkSender` on both the delivery and the forward direction; the delivery-report event path end to end; and the chunk grammar, directly and through multi-chunk transfers.
- **With `--all-features` only:** the SDK's handshake, event loops, sinks, streaming writers and pull path for all four APIs; its decline-then-redelivery path, a reply issued through the sink from inside a pulled delivery, its deferred forwarding-outcome path, the registration-time rejections it surfaces (`NoDtnNodeId`, an over-declared lane count, the reserved drop reason), and its disconnection paths on both a server pool shutdown and a killed transport.
- **Not reached by any test:** `server/status.rs` driven directly, so only the arms a wire scenario happens to hit execute; the SDK's `DuplicateBundle` and `Dropped` arms of `transfer_error`; the per-session `MAX_INBOUND_TRANSFERS` wait; the collection that arrives as its announcement is withdrawn; a withdrawal over a live connection, which is pinned as a unit (RVZ-06) but never observed on the wire; the `Reflect` route-action conversion; and the transmission-flag `SendOptions` conversions beyond the delivery-report flag.

## 5. Test Infrastructure

- **Shared fixtures live in `tests/common/mod.rs`,** cross-imported by the five wire binaries: `timeout` (a 10 second regression failsafe), `ipn1`, `build_bundle`, `build_bpa`, `serve`, and `UnregisterWatch` with `wait_unregistered`. `UnregisterWatch` is a `BpaRegistration` decorator that broadcasts once the BPA has actually run a component's `on_unregister`, which is what lets a test re-register a service id without racing its predecessor's release. The inline suites keep their own copies of `timeout`, `ipn1`, `build_bpa` and `serve` in `server/services/mod.rs`, because an integration-test helper module is not reachable from `src/`.
- **Per-binary harness, no shared harness type:** each wire binary defines its own `Harness` holding the `Arc<Bpa>`, the `TaskPool`, a connected generated client, the bound address and the `UnregisterWatch`, built by `harness()` and, where a test needs to reach a stage or read back a bound, `harness_with_limits()` (the application binary also has `harness_with()` for a different node id set). Each builds a real `Bpa` on `ipn:1` with in-memory storage, mounts the server behind a tonic server on a port-0 listener, and connects a client. The application and service binaries enable status reports because their delivery-report tests need them; the CLA, routing and lifecycle binaries disable them.
- **The BPA is built with the `no-rfc9171-autoregister` dev feature,** so the auto-registered RFC 9171 validity filter does not sit between the wire and the assertions. The gRPC servers, not ingress policy, are the subject under test.
- **Per-binary helpers wrap the doors.** Application: `register`, `register_with_chunk_size`, `register_with`, `send`, `send_chunked`, `chunked` and `chunked_at`, `metadata`, `collect`, `collect_chunks`, `collect_unacked`, `take_all`, `terminal_status`, `clean_end`, `delivery` and `recollected_after_reregistration`. Service: `register`, `send`, `collect` and `delivery`. CLA: `tcp_register`, `subscribe`, `register`, `register_with`, `tcp_address`, `add_peer`, `dispatch`, `dispatch_chunked`, `forwarding`, `execute_forward`, `accepted_forwarding`, `report_outcome` and `reforwarded_after_reregistration`. Routing: `register`, `via`, `drop_with_reason`, `add_route` and `remove_route`. `drain_undelivered` (application and service) and `assert_never_forwards` (CLA) are the negative-assertion helpers: each reads its stream to the `UNAVAILABLE` ending and asserts no announcement arrived, rather than proving absence with a quiet window.
- **Timing discipline:** the watchdog suite and the bounded reader's rate-rule tests (RCV-01 to RCV-06) are the only places a real duration appears, and they run on a paused clock (`start_paused`), racing the stall against a `sleep` inside a `select!` so the clock advances to whichever fires first. Every wire test that reaches a stage sets that stage's limit to `Duration::ZERO`, and the two handshake tests set `handshake` to zero. Everything else is event-driven, with `timeout()` used solely as a hang failsafe. Wire tests run on the multi-threaded runtime (`worker_threads = 2`), including most zero-limit stall tests; the two exceptions, APP-26 and APP-27, and the inline units run current-thread, apart from the two endpoint tests that mount a server.
- **Fakes are minimal and local:** `ParkedBpa` and `ParkedSink` in the application binary park inside `register_application` so a test can abandon an rpc inside the commit window; `SinkOnly` there originates a bundle in-process so a zero `idle` bound is not also applied to the `Send`; `StalledProducer` in the lifecycle binary is a segment source that never yields; `killable_proxy` there interposes a TCP proxy a test can kill. The SDK test components (`SdkApp`, `DecliningApp`, `EchoApp`, `SdkService`, `AcceptingCla`, `SdkCla`, `OverLanedCla`, `SdkAgent`, and `LifecycleApp` with its four delivery modes) store their sink and report observations over a channel, following the same event-driven pattern.

## 6. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Transport | A long run of small calls on one connection is never issued | High | The h2 small-frame budget failure found in review kills a whole connection at around the 100th short call. No scenario issues enough calls on one channel to see it, so the suite cannot catch a regression of it |
| Session bounds | The per-session `MAX_INBOUND_TRANSFERS` bound is untested | Medium | No test opens four concurrent `Send` or `Dispatch` calls on one session and shows that a fifth waits for one of them to end rather than failing or running; the bound is only pinned arithmetically through `SESSION_FOOTPRINT` (LIM-02) |
| Rendezvous | The announce/collect race is untested | Medium | A collection that wins the hand-over as the announcement is withdrawn is ended `ABORTED` (`the bundle was withdrawn`) through the slot it holds; no unit or wire test drives a `collect` into that window |
| Client SDK | `transfer_error` mappings partially tested | Medium | The `StreamCancelled` and disconnection arms execute; the `DuplicateBundle` (`ALREADY_EXISTS`) and `Dropped` (`FAILED_PRECONDITION`) arms do not, because no scenario sends a duplicate or filter-refused bundle through the SDK. The carried-whole session ending is pinned only on the application API (LIF-08), so the identical CLA and routing paths are unexercised |
| Lifecycle | Silent transport death unpinned | Medium | Needs a transport that can go quiet without closing. The proxy kills connections outright (LIF-08) but cannot leave one silently half-open, which is what keepalive-bounded detection needs |
| Status mapping | `server/status.rs` has no test of its own | Low | The arms a wire scenario reaches execute through it. The arms with no BPA path to them (`DtnInvalidServiceName`, `NoIpnNodeId`, `NoDtnNodeId`, which the BPA never constructs) and the arms no scenario reaches (`AdministrativeEndpoint`, `PayloadUnaddressable`, `Dropped`, `DuplicateBundle`, `NullNextHop`, `ViaOwnNode`) are unexercised, so their code and message are pinned by nothing (plan `STS-01`) |
| Data plane | Mid-transfer withdrawal untested over the wire | Low | RVZ-06 pins the `ABORTED` the writer breaks with. No wire test expires or deletes a bundle under a live collection |
| Lifecycle | CLA vanished-client mid-`Forward` residue unpinned | Low | Abandonment and dropped-session teardown are pinned (CLA-03, CLA-16); a client vanishing with the rendezvous claimed and chunks in flight should be pinned end to end once a killable transport reaches the CLA binary |
| Wire options | `Reflect` route action and non-report `SendOptions` flags unasserted | Low | The remaining route-action conversion and the transmission flags beyond the delivery-report flag have no test |

## 7. Conclusion

The crate carries 151 tests and 5 doctests: 45 inline units and 106 integration tests, of which 11 drive the client SDK against the servers from the four wire suites and 11 more are the SDK lifecycle binary. 148 execute; the three that do not each assert a promise the v1 wire makes and the BPA does not yet keep, and [`TODO.md`](TODO.md) names the change that un-ignores each one. Every planned scenario is implemented except the status-mapping unit (STS-01) and the three deferred scenarios that need infrastructure the in-process suite does not have.

Instrumented line coverage is the generated figure in `docs/coverage_summary.md`; section 4 gives the command to regenerate it and an inventory-based assessment of what the tests reach.

The strength is the doctrine: every wire test runs the real wire against a real `Bpa`, every API pins its registration, its token discipline, its truncation-never-commits rule and its abandonment-defers-work rule, the ack-gated delivery commit and the deferred forwarding outcome are pinned in every arm, every bound the server announces is pinned against what it enforces in both directions, every door's opening is pinned to the status it fails with, and all five watchdog stages are pinned twice, once in units over a paused clock and once over the wire with a zero limit. The primary gap is the one the suite structurally cannot see today: a connection that dies after a long run of small calls. Below that sit the untested per-session transfer bound, the announce/collect race, the SDK's `DuplicateBundle` and `Dropped` mappings, the killable-transport lifecycle scenarios, and the status mapping's unreached arms.
