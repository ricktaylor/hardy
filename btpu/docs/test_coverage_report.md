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
| -04 §3.2, §8.5, §8.6 | Padding | Pass | `codec.rs::pad_pdu_fills_to_target`, `indefinite_padding_skipped` | 3.3, 3.5 |
| -04 §4, §8.1–8.3 | Segmentation and reassembly | Pass | `sender.rs::large_bundle_segmented`, `receiver.rs::out_of_order_completes_on_end_recheck` | 3.7, 3.10 |
| -04 §4.1 | Interleaving | Not tested | Not implemented by the sender; the receiver accepts interleaved transfers (`receiver.rs::window_wraparound_with_live_transfers`) | — |
| -04 §4.2, §8.4 | Transfer cancellation | Pass | `sender.rs::cancel_after_partial_emission_discards_the_rest_and_queues_a_cancel`, `receiver.rs::cancel_of_an_in_window_transfer_before_its_segments_is_remembered` | 3.9, 3.11 |
| -04 §5 | Transfer window | Pass | `transfer.rs::new_transfer_boundary_is_half_space_plus_half_window`, `window_gates_on_span_not_count` | 3.6, 3.11 |
| -04 §6 | Repeated messages | Pass (receive) | `receiver.rs::duplicate_segment_dropped_and_first_copy_kept`; the sender does not repeat | 3.10 |
| -04 §7, §7.1 | Message header and flags | Pass | `codec_header.rs::wire_format_layout`, `codec_message.rs::message_flags_nibble_round_trips_all_bits` | 3.1 |
| -04 §7.2, §9.1 | Hint items, Bundle Length hint | Pass | `codec_hint.rs::round_trip_bundle_length_every_width`, `sender.rs::first_segment_has_bundle_length_hint` | 3.2, 3.7 |
| -04 §7.3 | Unrecognised messages and hints | Pass | `codec.rs::unknown_message_with_hints_relays_intact`, `codec_hint.rs::unknown_hint_preserved` | 3.2, 3.3 |
| -04 §10 | Resource limits | Pass | `receiver.rs::tiny_segment_flood_is_rejected_as_too_fragmented`, `transfer_that_would_exceed_the_retention_limit_is_rejected` | 3.15, 3.16 |
| -04 §12.1 | Bare and encapsulated bundles | Pass | `codec.rs::bare_bpv7_bundle_decoded_as_bundle_message`, `extent_hook_delivers_mid_pdu_bundle_and_iteration_continues` | 3.4, 3.8 |
| fec-02 §3, §4 | FEC message framing and transfer rules | Pass | `codec.rs::round_trip_fec_messages_with_fec_decoding_on`, `receiver.rs::changed_fec_instance_id_rejects_the_transfer` | 3.3, 3.12 |

## 2. Test Inventory

### Unit Tests

Test names are listed per scenario in [`UTP-BTPU-01`](unit_test_plan.md) Section 3; this table groups them by file.

| File | Tests | Plan Section | Scope |
| :--- | :--- | :--- | :--- |
| `tests/codec_header.rs` | 8 | 3.1 | Header encode/decode, 20-bit length bound |
| `tests/codec_message.rs` | 11 | 3.1 | Flags, type registry, frame classification |
| `tests/codec_hint.rs` | 17 | 3.2 | Hint items, Bundle Length widths, `Hints` set |
| `tests/codec.rs` | 32 | 3.3, 3.4, 3.5 | PDU decoding, fault containment, bare frames, extent hook, padding |
| `tests/transfer.rs` | 23 | 3.6, 3.17 | Window classification, allocator, `WindowSize` |
| `transfer.rs` (inline) | 3 | 3.6 | Window keys and expiry across the roll-over |
| `tests/sender.rs` | 55 | 3.7, 3.8, 3.9, 3.17 | Admission, segmentation, hints, packing, carried list, cancellation, sender configuration |
| `sender.rs` (inline) | 2 | 3.7, 3.8 | Unsegmented ID wrap, packing progress |
| `tests/receiver.rs` | 102 | 3.10 to 3.17, 3.19 | Reassembly, window, cancel, FEC rules, events, hints, limits, receiver configuration, round trips |
| `receiver.rs` (inline) | 9 | 3.11, 3.15, 3.16 | Closed-map pruning, reset, overhead and retention accounting |
| `tests/config.rs` | 4 | 3.17 | serde form (`serde` feature) |
| `tests/tower.rs` | 16 | 3.18 | `Service`/`Stream` adapters (`tower` feature) |
| `tests/tunnel.rs` | 3 | 3.19 | Packet tunnel over lossless, lossy, and UDP loopback links |
| Doctests | 3 | 3.19 | README example (`rand` feature), `codec::BundleExtent`, `receiver::ReceiverConfig` |

**Total: 288 tests (14 inline, 271 integration, 3 doctests) with all features; 261 with default features.** The 27 feature-gated tests are 16 `tower`, 4 `serde`, 6 `rand` seeding tests, and the README doctest.

### Fuzz Tests

| Target | File | Status |
| :--- | :--- | :--- |
| `decode` | `fuzz/fuzz_targets/decode.rs` | Implemented — one PDU under three decode configurations; re-encode and round-trip invariants |
| `receive` | `fuzz/fuzz_targets/receive.rs` | Implemented — a PDU sequence into one receiver under 16 configurations; delivery and event-order invariants |

**Total: 2 fuzz targets.**

## 3. Coverage vs Plan

Cross-reference against [`UTP-BTPU-01`](unit_test_plan.md):

| Section | Scenario | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| 3.1 Message Header and Type Space | Header round trip, bounds, layout, flags, registry, frame classification | 7 | 7 | Complete |
| 3.2 Hint Items and Hint Sets | Bounds, widths, chains, unknown hints, folding, `Hints` | 6 | 6 | Complete |
| 3.3 PDU Decoding and Fault Containment | Core, FEC, padding, unknown relay, RFU flags, faults | 8 | 8 | Complete |
| 3.4 Bare and Encapsulated Bundles | Bare frame, hook, undelimitable bundle | 3 | 3 | Complete |
| 3.5 Encoding and Padding | Pad to target, encode failure | 2 | 2 | Complete |
| 3.6 Transfer Window and Number Allocation | Classification, roll-over, expiry, allocator, seeding | 5 | 5 | Complete |
| 3.7 Sender: Admission, Segmentation, and Hints | Admission, segmentation, hints, identifiers | 4 | 4 | Complete |
| 3.8 Sender: Packing and Link Framing | Framing, progress, carried list, debug output | 6 | 6 | Complete |
| 3.9 Sender: Window Release and Cancellation | Release, cancel transfer, cancel bundle, ID matching | 4 | 4 | Complete |
| 3.10 Receiver: Reassembly and Repeats | Delivery, conflicts, repeats, empty bundles, copy policy | 5 | 5 | Complete |
| 3.11 Receiver: Window, Cancellation, and Reset | Cancel, expiry, pruning, reset | 4 | 4 | Complete |
| 3.12 Receiver: FEC Transfer Rules | Mixing, configuration change, window and limits | 3 | 3 | Complete |
| 3.13 Receiver: Events and Fault Containment | Prior events kept, event list, debug output | 3 | 3 | Complete |
| 3.14 Receiver: Hints, Bare Frames, and the Extent Hook | Delivered hints, bare frames and hook | 2 | 2 | Complete |
| 3.15 Receiver: Per-Transfer Limits | Cap, budget, link-derived limit, enforcement | 4 | 4 | Complete |
| 3.16 Receiver: Retention Limit | Sizing, enforcement, accounting | 3 | 3 | Complete |
| 3.17 Configuration Types | Sender, receiver, window size, serde | 4 | 4 | Complete |
| 3.18 Tower Adapters | Services, backpressure, wakeups, stream | 4 | 4 | Complete |
| 3.19 End to End | Round trip, tunnel, doc examples | 3 | 3 | Complete |
| **Total** | | **80** | **80** | **100%** |

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
cargo +nightly cov -- export --format=lcov ...
lcov --summary ./fuzz/coverage/<target>/lcov.info
```

The two layers check different things. The unit tests pin specified behaviour to known inputs: what the receiver delivers, drops, or rejects, and why. The fuzz targets check that no input panics the codec or receiver, that the codec's encoder and decoder agree on everything the decoder yields, and that the receiver's size and event-order guarantees hold for any PDU sequence.

## 5. Test Infrastructure

* **Fixtures:** `tests/common/mod.rs` holds the shared builders: configurations and endpoints (`sender_config`, `receiver_config`, `sender`, `receiver`), messages (`bundle_msg`, `segment`, `end`, `cancel`), expected events (`received`, `received_with`), payloads (`bundle`, `bpv7_like`), and `encode`. Each integration-test binary imports it with `mod common;`.
* **Inline tests** cover crate-private state that the public API cannot observe: the receiver's closed-transfer map and retention charges, the window's expiry keys, and the sender's packing loop.
* **Determinism:** no test sleeps or depends on timing. The `tower` tests poll by hand and observe wakeups through a waker that raises a flag; `tests/tunnel.rs` runs its UDP loopback in lockstep, with one datagram in flight, and uses a read timeout only to bound a regression.
* **Feature gates:** tests that need `tower`, `serde`, or `rand` are gated on that feature, so `cargo test -p hardy-btpu` passes without them.

## 6. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Interleaving (-04 §4.1) | Sender sends each transfer to completion before the next | Low | Optional in the draft; the receiver side is covered |
| Repetition (-04 §6) | Sender never repeats messages | Low | Optional in the draft; the receiver's duplicate handling is covered |
| FEC schemes | No FEC encoding or repair | Low | Out of scope for fec-02's framing; only framing and transfer rules are tested |
| `no_std` targets | Thumb builds run in CI only | Low | `thumbv7em-none-eabihf`, and `thumbv6m-none-eabi` with `critical-section` |
| Fuzz corpus | No seed corpus checked in | Low | See [`FUZZ-BTPU-01`](fuzz_test_plan.md) Section 7 |
| Interoperability | No test against another BTP-U implementation | Medium | No other implementation is available to test against |

## 7. Conclusion

The BTP-U crate has 288 tests (271 integration, 14 inline, 3 doctests) and 2 fuzz targets, covering 80 of 80 planned scenarios (100%). No LLRs are assigned; every implemented section of `draft-ietf-dtn-btpu-04` and the framing and transfer rules of `draft-ietf-dtn-btpu-fec-02` trace to passing tests. Line coverage is generated into the [coverage summary](../../docs/coverage_summary.md). The strongest coverage is fault containment in the codec and the receiver's bounded-memory limits, which are also the fuzz targets' focus. The remaining gaps are the optional sender behaviours (interleaving and repetition), FEC schemes, and the lack of an interoperability peer.
