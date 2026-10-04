# BTP-U Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-btpu` |
| **Crate version** | `0.1.0` |
| **Standard** | `draft-ietf-dtn-btpu-04` — Bundle Transfer Protocol - Unidirectional; `draft-ietf-dtn-btpu-fec-02` — FEC extension (message framing) |
| **Test Plans** | [`UTP-BTPU-01`](unit_test_plan.md), [`FUZZ-BTPU-01`](fuzz_test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

No LLRs are assigned to this crate. BTP-U is a pre-standard IETF extension and falls under the REQ-4 goal, whose Part 4 profiles (4.1 to 4.10) do not include it; fuzzing is under REQ-14 (Part 4 ref 14.1). Verification is traced to the drafts instead:

| Draft Section | Feature | Result | Test | Plan Section |
| :--- | :--- | :--- | :--- | :--- |
| -04 §3.2, §8.5, §8.6 | Padding | Pass | `codec.rs::pad_pdu_fills_to_target`, `indefinite_padding_skipped`, `sender.rs::a_short_pdu_is_padded_up_to_the_floor` | 3.3, 3.5, 3.8 |
| -04 §4, §8.1–8.3 | Segmentation and reassembly | Pass | `sender.rs::large_bundle_segmented`, `receiver.rs::out_of_order_completes_on_end_recheck`, `streaming.rs::a_filled_gap_releases_the_run_behind_it` | 3.7, 3.10, 3.17 |
| -04 §4.1 | Interleaving | Pass | The sender passes over a transfer that cannot supply (`send_handle.rs::a_transfer_waiting_on_its_producer_is_passed_over`, `send_handle.rs::a_transfer_passed_over_fills_the_tail_once_the_room_shrinks_to_its_bytes`, `sender.rs::a_transfer_without_room_in_the_tail_is_passed_over_but_a_message_is_not`); priority interleaving is not implemented; the receiver accepts interleaved transfers (`receiver.rs::window_wraparound_with_live_transfers`) | 3.8 |
| -04 §4.2, §8.4 | Transfer cancellation | Pass | `sender.rs::cancel_after_partial_emission_discards_the_rest_and_queues_a_cancel`, `receiver.rs::cancel_of_an_in_window_transfer_before_its_segments_is_remembered`, `a_cancel_longer_than_four_octets_is_honoured` | 3.9, 3.11 |
| -04 §5 | Transfer window | Pass | `transfer.rs::new_transfer_boundary_is_half_space_plus_half_window`, `window_gates_on_span_not_count` | 3.6, 3.11 |
| -04 §6 | Repeated messages | Pass (receive) | `receiver.rs::duplicate_segment_dropped_and_first_copy_kept`; the sender does not repeat | 3.10 |
| -04 §7, §7.1 | Message header and flags | Pass | `codec_header.rs::wire_format_layout`, `codec_message.rs::message_flags_nibble_round_trips_all_bits` | 3.1 |
| -04 §7.2, §9.1 | Hint items, Bundle Length hint | Pass | `codec_hint.rs::round_trip_bundle_length_every_width`, `sender.rs::first_segment_has_bundle_length_hint` | 3.2, 3.7 |
| -04 §7.3 | Unrecognised messages and hints | Pass | `codec.rs::unknown_message_with_hints_relays_intact`, `codec_hint.rs::unknown_hint_preserved` | 3.2, 3.3 |
| -04 §10 | Resource limits | Pass | `receiver.rs::tiny_segment_flood_is_rejected_as_too_fragmented`, `transfer_that_would_exceed_the_retention_limit_is_rejected`, `budget.rs::receivers_sharing_a_budget_are_bounded_together` | 3.15, 3.16, 3.18 |
| -04 §12.1 | Bare and encapsulated bundles | Pass | `codec.rs::bare_bpv7_bundle_decoded_as_bundle_message`, `extent_hook_delivers_mid_pdu_bundle_and_iteration_continues` | 3.4, 3.8 |
| fec-02 §3, §4 | FEC message framing and transfer rules | Pass | `codec.rs::round_trip_fec_messages_with_fec_decoding_on`, `receiver.rs::changed_fec_instance_id_rejects_the_transfer` | 3.3, 3.12 |

## 2. Test Inventory

### Unit Tests

Test names are listed per scenario in [`UTP-BTPU-01`](unit_test_plan.md) Section 3; this table groups them by file.

| File | Tests | Plan Section | Scope |
| :--- | :--- | :--- | :--- |
| `tests/codec_header.rs` | 8 | 3.1 | Header encode/decode, 20-bit length bound |
| `tests/codec_message.rs` | 11 | 3.1 | Flags, type registry, frame classification |
| `tests/codec_hint.rs` | 19 | 3.2 | Hint items, Bundle Length widths, `Hints` set |
| `tests/codec.rs` | 33 | 3.3, 3.4, 3.5 | PDU decoding, fault containment, bare frames, extent hook, padding |
| `tests/transfer.rs` | 4 | 3.19 | `WindowSize`, the window-full error |
| `transfer.rs` (inline) | 21 | 3.6, 3.19 | Window classification, allocator, transfer ids and expiry across the roll-over and across a reset |
| `tests/sender.rs` | 69 | 3.7, 3.8, 3.9, 3.19 | Admission, segmentation, hints, packing, carried list, cancellation, foreign IDs, sender configuration |
| `tests/send_handle.rs` | 30 | 3.7, 3.8, 3.9 | Bundles pushed through a `SendHandle`, push errors, foreign handles, segment cut strategy, push readiness, passing over waiting transfers, flush, cancellation, dropped handles and `outstanding`, the high watermark |
| `sender/mod.rs` (inline) | 2 | 3.7, 3.8 | Unsegmented ID wrap, queued-byte overflow |
| `sender/segmenter.rs` (inline) | 5 | 3.7, 3.8 | Segment cutting from pushed chunks, the flush index check, the starved and retry-room predicates |
| `sender/pack.rs` (inline) | 2 | 3.8 | Packing progress, packing cost behind starved transfers |
| `tests/receiver.rs` | 103 | 3.10 to 3.16, 3.19, 3.21 | Reassembly, window, cancel, FEC rules, events, hints, limits, receiver configuration, round trips |
| `receiver/mod.rs` (inline) | 10 | 3.11, 3.15, 3.16, 3.18 | Closed-map pruning, reset, overhead and retention accounting, budget accounting |
| `tests/streaming.rs` | 24 | 3.17 | Streamed delivery, its ending events and hints, refusal, foreign ids |
| `tests/budget.rs` | 9 | 3.18 | Shared retention budget and CLA charges |
| `tests/config.rs` | 4 | 3.19 | serde form (`serde` feature) |
| `tests/tower.rs` | 21 | 3.20 | `Service`/`Stream` adapters (`tower` feature) |
| `tests/tunnel.rs` | 3 | 3.21 | Packet tunnel over lossless, lossy, and UDP loopback links |
| Doctests | 3 | 3.21 | README example (`rand` feature), `codec::BundleExtent`, `receiver::ReceiverConfig` |

**Total: 381 tests (40 inline, 338 integration, 3 doctests) with all features; 352 with default features.** The 29 feature-gated tests are 21 `tower`, 4 `serde`, 3 `rand` seeding tests, and the README doctest.

### Fuzz Tests

| Target | File | Status |
| :--- | :--- | :--- |
| `decode` | `fuzz/fuzz_targets/decode.rs` | Implemented — one PDU under three decode configurations; re-encode and round-trip invariants |
| `receive` | `fuzz/fuzz_targets/receive.rs` | Implemented — a PDU sequence into one receiver under 128 configurations; delivery, streaming, budget, and event-order invariants |
| `send` | `fuzz/fuzz_targets/send.rs` | Implemented — begin, push, finish, enqueue, cancel, and drain (with and without flush) on one sender under 32 configurations and 256 PDU sizes, into a receiver over a lossless link; delivery, carried-list, PDU-size, push-readiness, and drain-to-empty invariants |

**Total: 3 fuzz targets.**

## 3. Coverage vs Plan

Cross-reference against [`UTP-BTPU-01`](unit_test_plan.md):

| Section | Scenario | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| 3.1 Message Header and Type Space | Header round trip, bounds, layout, flags, registry, frame classification | 7 | 7 | Complete |
| 3.2 Hint Items and Hint Sets | Bounds, widths, chains, unknown hints, folding, `Hints` | 6 | 6 | Complete |
| 3.3 PDU Decoding and Fault Containment | Core, FEC, padding, unknown relay, RFU flags, faults | 8 | 8 | Complete |
| 3.4 Bare and Encapsulated Bundles | Bare frame, hook, undelimitable bundle | 3 | 3 | Complete |
| 3.5 Encoding and Padding | Pad to target, encode failure | 2 | 2 | Complete |
| 3.6 Transfer Window and Number Allocation | Classification, roll-over, expiry, allocator | 4 | 4 | Complete |
| 3.7 Sender: Admission, Segmentation, and Hints | Admission, segmentation, pack-time cutting, send handle, segment cut strategy, push errors, high watermark, push readiness, hints, identifiers | 10 | 10 | Complete |
| 3.8 Sender: Packing and Link Framing | Framing, padding floor, passing over transfers, flush, progress, packing cost, carried list, debug output | 10 | 10 | Complete |
| 3.9 Sender: Window Release and Cancellation | Release, cancel transfer, cancel bundle, cancel pushed bundle, ID matching | 5 | 5 | Complete |
| 3.10 Receiver: Reassembly and Repeats | Delivery, conflicts, repeats, empty bundles, copy policy | 5 | 5 | Complete |
| 3.11 Receiver: Window, Cancellation, and Reset | Cancel, expiry, pruning, reset | 4 | 4 | Complete |
| 3.12 Receiver: FEC Transfer Rules | Mixing, configuration change, window and limits | 3 | 3 | Complete |
| 3.13 Receiver: Events and Fault Containment | Prior events kept, event list, debug output | 3 | 3 | Complete |
| 3.14 Receiver: Hints, Bare Frames, and the Extent Hook | Delivered hints, bare frames and hook | 2 | 2 | Complete |
| 3.15 Receiver: Per-Transfer Limits | Cap, budget, link-derived limit, enforcement | 4 | 4 | Complete |
| 3.16 Receiver: Retention Limit | Sizing, enforcement, accounting | 3 | 3 | Complete |
| 3.17 Receiver: Streamed Delivery and Refusal | In-order release, reordering, empty segments, ending, released segments, hints, FEC, drop ids, refusal | 9 | 9 | Complete |
| 3.18 Receiver: Shared Retention Budget | Charges, sharing, reason order, joining and leaving, accounting | 5 | 5 | Complete |
| 3.19 Configuration Types | Sender, receiver, window size, serde | 4 | 4 | Complete |
| 3.20 Tower Adapters | Services, backpressure, wakeups, stream | 4 | 4 | Complete |
| 3.21 End to End | Round trip, tunnel, doc examples | 3 | 3 | Complete |
| **Total** | | **104** | **104** | **100%** |

## 4. Line Coverage

> Current figures are generated — see the [coverage summary](../../docs/coverage_summary.md) (refreshed by `scripts/run_lcov.sh`) and the live coverage dashboards (CFLite fuzz coverage on gh-pages; CI-published coverage planned). Figures include the integration tests.

```
cargo llvm-cov test --package hardy-btpu --all-features --lcov --output-path lcov.info --html
lcov --summary lcov.info
```

### Fuzz Coverage

```
cd btpu
cargo +nightly fuzz coverage decode
cargo +nightly fuzz coverage receive
cargo +nightly fuzz coverage send
cargo +nightly cov -- export --format=lcov ...
lcov --summary ./fuzz/coverage/<target>/lcov.info
```

The two layers check different things. The unit tests pin specified behaviour to known inputs: what the receiver delivers, drops, or rejects, and why. The fuzz targets check that no input panics the codec or receiver, that the codec's encoder and decoder agree on everything the decoder yields, that the receiver's size and event-order guarantees hold for any PDU sequence, and that any sequence of sender calls delivers exactly the bundles neither cancelled nor finished short, once each, without a producer gated on push readiness ever waiting forever.

## 5. Test Infrastructure

* **Fixtures:** `tests/common/mod.rs` holds the shared builders: configurations and endpoints (`sender_config`, `receiver_config`, `sender`, `receiver`), messages (`bundle_msg`, `segment`, `end`, `cancel`), expected events (`received`, `received_with`, `dropped`, `rejected`, `expired`, `cancelled`, `none`) as an `Event` mirror of `ReceiverEvent` that names transfers by number, since a `TransferId` cannot be constructed outside the crate, payloads (`bundle`, `bpv7_like`), `encode`, and `is_within`. Each integration-test binary imports it with `mod common;`.
* **Inline tests** cover crate-private state that the public API cannot observe: the receiver's closed-transfer map, retention charges, and budget share, the window's transfer ids, and the sender's packing loop and pushed-chunk release.
* **Determinism:** no test sleeps or depends on timing. The `tower` tests poll by hand and observe wakeups through a waker that raises a flag; `tests/tunnel.rs` runs its UDP loopback in lockstep, with one datagram in flight, and uses a read timeout only to bound a regression.
* **Feature gates:** tests that need `tower`, `serde`, or `rand` are gated on that feature, so `cargo test -p hardy-btpu` passes without them.

## 6. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Priority interleaving (-04 §4.1) | Sender interleaves only by passing over a transfer that cannot supply; a ready transfer goes out ahead of everything behind it | Low | Optional in the draft; the receiver side is covered |
| Repetition (-04 §6) | Sender never repeats messages | Low | Optional in the draft; the receiver's duplicate handling is covered |
| FEC schemes | No FEC encoding or repair | Low | Out of scope for fec-02's framing; only framing and transfer rules are tested |
| `no_std` targets | Thumb builds run in CI only | Low | `thumbv7em-none-eabihf`, and `thumbv6m-none-eabi` with `critical-section` |
| Fuzz corpus | No seed corpus checked in | Low | See [`FUZZ-BTPU-01`](fuzz_test_plan.md) Section 7 |
| Interoperability | No test against another BTP-U implementation | Medium | No other implementation is available to test against |

## 7. Conclusion

The BTP-U crate has 381 tests (338 integration, 40 inline, 3 doctests) and 3 fuzz targets, covering 104 of 104 planned scenarios (100%). No LLRs are assigned; every implemented section of `draft-ietf-dtn-btpu-04` and the framing and transfer rules of `draft-ietf-dtn-btpu-fec-02` trace to passing tests. Line coverage is generated into the [coverage summary](../../docs/coverage_summary.md). The strongest coverage is fault containment in the codec and the receiver's bounded-memory limits, which are also the fuzz targets' focus. The remaining gaps are the optional sender behaviours (interleaving and repetition), FEC schemes, and the lack of an interoperability peer.
