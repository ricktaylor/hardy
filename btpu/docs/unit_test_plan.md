# Unit Test Plan: BTP-U

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Convergence Layer Protocol Engine |
| **Module** | `hardy-btpu` |
| **Requirements Ref** | [REQ-4](../../docs/requirements.md#req-4-alignment-with-on-going-dtn-standardisation), [REQ-14](../../docs/requirements.md#req-14-reliability) |
| **Standard Ref** | `draft-ietf-dtn-btpu-04`, `draft-ietf-dtn-btpu-fec-02` |
| **Test Suite ID** | UTP-BTPU-01 |
| **Version** | 1.0 |

## 1. Introduction

This document details the unit testing strategy for the `hardy-btpu` module, a `no_std` implementation of the Bundle Transfer Protocol - Unidirectional and the message framing of its FEC extension. The crate is a pure protocol library: a sender that segments and packs bundles into link-layer PDUs, a receiver that reassembles them under bounded memory, the codec both share, and optional `tower` adapters.

**Scope:**

* **Codec:** message header, type registry, hint items, PDU decoding with fault containment, bare and encapsulated bundle frames, padding.

* **Transfer Window:** the Section 5 window and the sender's transfer-number allocator.

* **Sender:** admission, segmentation, hints, PDU packing under fixed-size and variable link framing, the carried-bundle list, window release, and cancellation.

* **Receiver:** reassembly, repeats, cancellation, window expiry, FEC transfer rules, event reporting, hints, and the per-transfer and receiver-wide memory limits.

* **Configuration:** the validated configuration newtypes and their serde form.

* **Adapters and end to end:** the `tower` `Service`/`Stream` adapters and sender-to-receiver scenarios, including a packet tunnel over lossless, lossy, and UDP loopback links.

No FEC scheme is implemented, so FEC coverage is limited to framing and the receiver's cancellation rules. Message repetition and interleaving are not implemented and have no tests.

## 2. Requirements Mapping

No Low-Level Requirements are assigned to this crate in **[requirements.md](../../docs/requirements.md)**. BTP-U is a pre-standard IETF convergence-layer protocol, which places it under REQ-4; the requirements document's REQ-4 profiles (4.1 to 4.5) do not name it. Verification is therefore traced to the draft sections below, and the crate's reading of points the drafts leave open is recorded under Standards Compliance in [design.md](design.md).

| Ref | Description | Draft Reference |
 | ----- | ----- | ----- |
| **REQ-4** | Alignment with on-going DTN standardisation: pre-standard extensions are verified by automated unit or component testing. | `draft-ietf-dtn-btpu-04` Sections 3 to 9, 12; `draft-ietf-dtn-btpu-fec-02` Section 3 |
| **REQ-14** | Every external API is checked so that incorrect or malicious input does not cause a component to abort. | Sections 7.3, 10; see also [`FUZZ-BTPU-01`](fuzz_test_plan.md) |

## 3. Unit Test Cases

Tests live in the crate's `tests/` directory (public API) and in inline `#[cfg(test)]` modules (crate-private state). Integration tests are named `tests/<file>.rs::<function>`; inline tests are named `<file>.rs::<function>`, relative to `src/`. Shared fixtures are in `tests/common/mod.rs`.

### 3.1 Message Header and Type Space (Sections 7, 7.1, 12.1, 12.3)

*Objective: Verify the 4-byte message header and the classification of type values and frame first bytes.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Header round trip** | `tests/codec_header.rs::round_trip_basic`, `round_trip_with_hint_flag`, `round_trip_zero_length`, `round_trip_max_length`, `all_message_types_round_trip` | Decoded header equals the encoded one |
| **Content length over 20 bits** | `tests/codec_header.rs::length_above_20_bits_is_refused` | **Error** |
| **Truncated header** | `tests/codec_header.rs::decode_insufficient_data` | `InsufficientData` |
| **Wire layout** | `tests/codec_header.rs::wire_format_layout` | Bytes match the Section 7 layout |
| **Flags nibble** | `tests/codec_message.rs::message_flags_nibble_round_trips_all_bits` | All 16 values round-trip |
| **Type registry** | `tests/codec_message.rs::from_byte_accepts_known_types`, `from_byte_rejects_reserved_and_unassigned_values`, `is_fec_covers_exactly_the_four_extension_types`, `is_reserved_bpv6_covers_value`, `is_reserved_bpv7_covers_range` | Defined, FEC, reserved, and unassigned values classified as Section 12.1 assigns them |
| **Frame first byte** | `tests/codec_message.rs::frame_kind_empty_is_btpu`, `frame_kind_bpv6`, `frame_kind_bpv7_full_range`, `frame_kind_known_btpu_types_classify_as_pdu`, `frame_kind_unallocated_btpu_space_classifies_as_pdu` | 6 and 0x80..0x9F classify as bundles, everything else as a BTP-U PDU |

### 3.2 Hint Items and Hint Sets (Sections 7.2, 7.3, 9.1)

*Objective: Verify hint encoding, the Bundle Length hint, unknown-hint preservation, and one-item-per-type folding.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Type and value bounds** | `tests/codec_hint.rs::hint_value_holds_at_most_what_the_length_field_declares`, `hint_type_holds_at_most_seven_bits`, `hint_type_reports_the_wire_type` | Out-of-range values refused |
| **Bundle Length widths** | `tests/codec_hint.rs::round_trip_bundle_length_every_width`, `bundle_length_uses_the_shortest_width_either_side_of_each_boundary` | Round-trips; shortest width chosen |
| **Hint chains** | `tests/codec_hint.rs::round_trip_chained_hints`, `encoded_len_matches_actual`, `truncated_chain_errors` | Chains round-trip at the predicted length; truncation is an **Error** |
| **Unknown and malformed hints** | `tests/codec_hint.rs::unknown_hint_preserved`, `malformed_bundle_length_size_is_carried_as_unknown` | Carried as unknown, byte-exact |
| **Repeat folding** | `tests/codec_hint.rs::repeated_types_fold_latest_wins_in_first_appearance_order`, `long_chain_of_repeats_folds_to_one_item` | One item per type, latest value |
| **`Hints` set** | `tests/codec_hint.rs::hints_keep_one_item_per_type_latest_wins_in_type_order`, `hints_compare_by_items_not_insertion_order`, `malformed_bundle_length_and_bundle_length_replace_each_other`, `hints_get_and_remove_by_type`, `hints_encoded_len_matches_the_encoder` | Set semantics by type; encoded length agrees with the encoder |

### 3.3 PDU Decoding and Fault Containment (Sections 3.1, 3.2, 7.3, 8)

*Objective: Verify that a PDU decodes to its messages, that unknown messages relay intact, and that a fault costs only what it must.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Core messages** | `tests/codec.rs::round_trip_core_messages`, `multiple_messages_in_pdu` | Every core type round-trips, several to a PDU |
| **FEC messages** | `tests/codec.rs::round_trip_fec_messages_with_fec_decoding_on`, `fec_types_relay_as_unknown_by_default` | Decoded when enabled, relayed as unknown otherwise |
| **Padding** | `tests/codec.rs::indefinite_padding_skipped`, `all_zeros_pdu` | Padding yields no messages |
| **Unknown message relay** | `tests/codec.rs::unknown_type_preserved`, `unknown_message_with_hints_relays_intact`, `unknown_message_rfu_flag_bits_relay_intact`, `malformed_hints_in_unknown_message_do_not_poison_pdu` | Re-encodes byte-exact |
| **Unknown construction** | `tests/codec.rs::unknown_cannot_carry_defined_or_reserved_type`, `unknown_encodes_exactly_the_types_the_decoder_reads_as_unknown` | Only unassigned types accepted |
| **RFU flags on a known type** | `tests/codec.rs::rfu_flag_bits_on_known_type_are_ignored` | Message decoded normally |
| **Interior fault** | `tests/codec.rs::malformed_interior_skips_only_that_message`, `short_bodies_are_contained_to_their_message`, `malformed_bundle_length_hint_keeps_the_segment` | That message skipped, iteration continues |
| **Framing fault** | `tests/codec.rs::length_past_buffer_is_terminal`, `truncation_at_every_offset_keeps_the_whole_messages_before_it` | Iteration ends; the prefix is kept |

### 3.4 Bare and Encapsulated Bundles (Sections 7.3, 12.1)

*Objective: Verify bundles whose first byte is a reserved type value, with and without the bundle-extent hook.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Bare frame** | `tests/codec.rs::bare_bpv6_bundle_decoded_as_bundle_message`, `bare_bpv7_bundle_decoded_as_bundle_message`, `bare_bundle_after_indefinite_padding_is_the_rest_of_the_frame`, `bare_frame_zero_fill_is_delivered_as_bundle_bytes_without_hook` | Rest of the frame delivered as one bundle |
| **Extent hook** | `tests/codec.rs::extent_hook_trims_bare_frame_padding`, `extent_hook_delivers_mid_pdu_bundle_and_iteration_continues` | Bundle delimited by the hook; iteration continues |
| **Undelimitable bundle** | `tests/codec.rs::mid_pdu_encapsulated_bundle_without_hook_is_terminal`, `extent_hook_declining_is_terminal`, `extent_hook_claiming_zero_bytes_is_terminal`, `extent_hook_overrunning_the_pdu_is_terminal` | Terminal **Error**; the prefix is kept |

### 3.5 Encoding and Padding (Sections 3.2, 8.5, 8.6)

*Objective: Verify PDU padding and encoder error behaviour.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Pad to target** | `tests/codec.rs::pad_pdu_fills_to_target`, `pad_pdu_small_remainder`, `pad_pdu_beyond_max_content_length_chains_messages` | Exactly the target length, chaining padding messages past the length-field maximum |
| **Encode failure** | `tests/codec.rs::encode_errors_leave_the_buffer_untouched` | **Error**; buffer unchanged |

### 3.6 Transfer Window and Number Allocation (Section 5)

*Objective: Verify the receiver's window classification and the sender's span-gated allocator, across the 2³² roll-over.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Classification** | `tests/transfer.rs::first_transfer_is_new`, `same_transfer_is_in_progress`, `sequential_transfers_advance`, `old_transfer_outside_window`, `new_transfer_boundary_is_half_space_plus_half_window`, `odd_window_size_rounds_the_margin_down` | New, in progress, or outside, per Section 5 |
| **Roll-over** | `tests/transfer.rs::wraparound`; `transfer.rs::keys_cover_exactly_the_window_behind_a_greatest_of_u32_max`, `a_key_expires_once_the_window_is_a_full_width_past_it`, `expiry_agrees_with_validity_across_the_wrap` | Same results across the wrap |
| **Expiry and reset** | `tests/transfer.rs::expired_transfers_detected`, `reset_forgets_the_greatest` | Expired numbers reported; reset forgets the greatest |
| **Allocator** | `tests/transfer.rs::allocate_sequential`, `allocator_wraps`, `release_of_oldest_frees_slot`, `window_gates_on_span_not_count`, `span_gate_survives_wraparound`, `release_of_unknown_number_is_ignored` | Allocation gated on the span of outstanding numbers |
| **Seeding** (`rand`) | `tests/transfer.rs::allocator_from_rng_seeds_first_number`, `allocator_try_from_rng_seeds_first_number`, `allocator_try_from_rng_returns_the_rng_error` | First number from the RNG; RNG error returned |

### 3.7 Sender: Admission, Segmentation, and Hints (Sections 4, 8.1 to 8.3, 9.1)

*Objective: Verify what `enqueue` accepts, how bundles are cut, and where hints travel.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Admission** | `tests/sender.rs::empty_bundle_rejected_at_enqueue`, `minimum_pdu_size_cannot_queue_an_undrainable_message`, `segmenting_floor_grows_with_the_bundle_length_hint`, `pdu_too_small_to_segment_leaves_window_untouched`, `max_pdu_size_bundle_encodes_without_panic` | Undeliverable bundles refused before taking a window slot |
| **Segmentation** | `tests/sender.rs::small_bundle_no_segmentation`, `large_bundle_segmented`, `a_segmented_transfer_is_one_queue_entry_until_its_end_is_packed` | A fitting bundle is one Bundle Message; a larger one is segments and an End |
| **Hints** | `tests/sender.rs::first_segment_has_bundle_length_hint`, `caller_hints_ride_first_segment_with_derived_bundle_length`, `repeated_caller_hint_types_go_out_once_latest_wins`, `first_segment_capacity_reduced_by_exactly_the_hint_bytes`, `caller_hints_ride_unsegmented_bundle_message`, `caller_hint_of_the_bundle_length_type_is_discarded_whatever_its_shape` | Hints on segment 0 or the Bundle Message; Bundle Length derived by the sender |
| **Identifiers** | `sender.rs::unsegmented_ids_wrap_across_message_and_bare`; `tests/sender.rs::from_rng_seeds_initial_transfer_number`, `try_from_rng_seeds_initial_transfer_number`, `try_from_rng_returns_the_rng_error` | IDs wrap; seeding as 3.6 |

### 3.8 Sender: Packing and Link Framing (Sections 3, 3.2, 7.3, 8.5)

*Objective: Verify PDU contents under each link framing, packing progress, and the carried-bundle list.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Variable framing** | `tests/sender.rs::variable_link_pdus_are_not_padded`, `variable_link_segmented_pdus_fill_the_pdu_except_the_last` | Unpadded PDUs sized to content |
| **Bare framing** | `tests/sender.rs::bare_framing_emits_the_bundle_bytes_alone_using_the_whole_pdu`, `bare_frames_keep_queue_order_and_never_share_a_pdu`, `bare_framing_keeps_hinted_bundles_framed`, `bare_framing_requires_a_bundle_reserved_first_byte`, `bare_bundles_count_against_send_queue_depth` | Eligible bundles alone in a PDU, in queue order |
| **Packing progress** | `sender.rs::oversized_queued_message_goes_out_alone_and_the_queue_moves_on` | Every PDU consumes at least one message |
| **Carried list** | `tests/sender.rs::pdu_lists_every_bundle_it_carries_and_flags_those_it_completes`, `middle_segments_list_their_transfer_as_incomplete`, `bare_frame_pdu_lists_its_bundle_as_complete`, `cancelled_transfer_is_never_listed_as_complete` | Every bundle with bytes listed; completion flagged once |
| **Carried list storage** | `tests/sender.rs::next_pdu_into_replaces_the_callers_list`, `next_pdu_list_moves_to_the_heap_only_past_the_inline_entries`, `reused_list_keeps_its_heap_buffer_for_a_pdu_that_would_fit_inline`, `with_capacity_below_the_inline_entries_stays_inline`, `list_sized_by_the_bundle_bound_holds_the_fullest_pdu_of_valid_bundles`, `lists_compare_by_entries_not_storage` | Inline up to four entries; reused buffers kept |
| **Debug output** | `tests/sender.rs::debug_summarises_the_queue_instead_of_printing_it` | No bundle bytes printed |

### 3.9 Sender: Window Release and Cancellation (Sections 4.2, 5, 8.4)

*Objective: Verify the self-releasing window and every `cancel` outcome.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Window release** | `tests/sender.rs::window_slot_is_released_when_the_transfer_end_is_packed`, `cancelling_the_newest_transfer_keeps_the_window_span` | Slot freed when the End is packed; span rule kept |
| **Cancel a transfer** | `tests/sender.rs::cancel_before_any_emission_queues_no_cancel_message`, `cancel_after_partial_emission_discards_the_rest_and_queues_a_cancel`, `cancel_goes_ahead_of_the_queued_backlog`, `cancel_after_the_last_bytes_are_packed_changes_nothing`, `bogus_cancel_is_a_noop` | Transfer Cancel only if part was emitted, at the queue front |
| **Cancel an unsegmented bundle** | `tests/sender.rs::cancel_removes_a_queued_bundle_message_and_sends_nothing_for_it`, `cancel_removes_a_queued_bare_frame`, `cancel_leaves_queued_bare_bundles_intact`, `cancelling_a_queued_bundle_frees_send_queue_depth` | Entry removed; nothing sent |
| **ID matching** | `tests/sender.rs::cancel_matches_the_id_variant_as_well_as_the_number`, `cancel_by_one_variant_leaves_the_others_with_that_number` | Only the named ID cancelled |

### 3.10 Receiver: Reassembly and Repeats (Sections 4, 6, 8.1 to 8.3)

*Objective: Verify delivery from any arrival order, sequence checks, repeat handling, and the copy policy.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Delivery** | `tests/receiver.rs::bundle_message_immediate`, `two_segment_transfer`, `out_of_order_completes_on_end_recheck`, `end_before_late_segment_completes_on_the_segment`, `empty_end_completes_the_transfer`, `empty_middle_segment_counts_toward_completion`, `final_segment_index_of_u32_max_leaves_the_transfer_open` | `Received` once all segments are held |
| **Sequence conflicts** | `tests/receiver.rs::conflicting_end_dropped_and_transfer_still_completes`, `segment_beyond_final_index_dropped`, `end_below_seen_segment_dropped` | `MessageDropped`; transfer unaffected |
| **Repeats** | `tests/receiver.rs::duplicate_segment_dropped_and_first_copy_kept`, `duplicate_end_dropped`, `duplicate_hints_are_not_applied`, `repeated_messages_of_a_delivered_transfer_do_not_redeliver` | `DropReason::Duplicate` or the closing reason; no second delivery |
| **Empty bundles** | `tests/receiver.rs::empty_bundle_message_rejected`, `transfer_with_no_data_is_rejected`, `empty_single_end_is_rejected_through_receive_pdu` | `BundleRejected` or `TransferRejected` (`Empty`); no bundle is zero bytes |
| **Copy policy** | `tests/receiver.rs::single_segment_transfer_shares_the_segment_bytes`, `segments_shorter_than_half_the_pdu_are_copied_out_of_it`, `retained_hint_values_are_copied_out_of_the_pdu` | Small fragments copied so no PDU is pinned |

### 3.11 Receiver: Window, Cancellation, and Reset (Sections 4.2, 5, 8.4)

*Objective: Verify Transfer Cancel handling, window expiry order, and the closed-transfer memory.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Transfer Cancel** | `tests/receiver.rs::repeated_cancel_is_idempotent`, `cancel_of_unknown_transfer_ignored`, `cancel_of_an_in_window_transfer_before_its_segments_is_remembered` | Applied in the window, remembered, ignored outside |
| **Window and expiry** | `tests/receiver.rs::outside_window_drop_reported`, `window_wraparound_with_live_transfers`, `transfers_straddling_the_wrap_expire_oldest_first`, `number_behind_a_just_wrapped_greatest_expires_before_it`, `number_more_than_half_the_space_ahead_advances_the_window` | `TransferExpired` oldest first, across the wrap |
| **Closed-transfer pruning** | `receiver.rs::closed_map_pruned_by_window_advance`, `closed_map_pruned_across_the_wrap` | Bounded by the window |
| **Reset** | `tests/receiver.rs::reset_forgets_delivered_transfers`, `reset_accepts_a_restarted_sender`; `receiver.rs::reset_clears_all_state` | All state cleared |

### 3.12 Receiver: FEC Transfer Rules (`draft-ietf-dtn-btpu-fec-02` Sections 3, 3.1, 3.2)

*Objective: Verify the cancellation rules that apply without an FEC scheme.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Core and FEC mixing** | `tests/receiver.rs::fec_message_on_core_transfer_rejects_it`, `core_message_on_fec_transfer_rejects_it`, `core_fec_mixing_rejects_through_receive_pdu` | `TransferRejected` (`FecCoreMixing`) |
| **Configuration change** | `tests/receiver.rs::fec_messages_with_one_configuration_keep_the_transfer_open`, `changed_fec_instance_id_rejects_the_transfer`, `changed_fec_encoding_id_rejects_the_transfer`, `switching_between_pre_agreed_and_explicit_fec_rejects_the_transfer` | `TransferRejected` (`FecConfigurationChanged`) |
| **Window, cancel, and limits** | `tests/receiver.rs::fec_transfer_expires_like_a_core_one`, `cancel_closes_an_fec_transfer`, `fec_types_do_not_touch_the_window_unless_enabled`, `fec_transfer_promising_an_oversized_bundle_is_rejected`, `fec_hint_bytes_count_against_retention` | FEC transfers share the core window and limits |

### 3.13 Receiver: Events and Fault Containment (Section 7.3)

*Objective: Verify that `receive_pdu` is infallible and that a fault never discards earlier events.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Faults keep prior events** | `tests/receiver.rs::malformed_message_mid_pdu_keeps_prior_events_and_continues`, `malformed_pdu_keeps_prior_events` | `MalformedMessage` or `MalformedPdu` after the earlier events |
| **Event list** | `tests/receiver.rs::every_message_in_a_pdu_can_produce_an_event`, `receive_pdu_into_replaces_the_callers_list`, `receive_pdu_into_reuses_the_callers_allocation` | One event per message at most; caller's allocation reused |
| **Debug output** | `tests/receiver.rs::debug_summarises_held_and_closed_transfers` | No bundle bytes printed |

### 3.14 Receiver: Hints, Bare Frames, and the Extent Hook (Sections 7.3, 9.1)

*Objective: Verify the hints reported with a bundle and the receive side of bare and encapsulated bundles.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Delivered hints** | `tests/receiver.rs::bundle_received_surfaces_transfer_hints`, `bundle_message_hints_deduped_latest_wins`, `bundle_length_hint_on_bundle_message_is_ignored`, `bundle_length_hint_smaller_than_the_data_still_delivers`, `malformed_bundle_length_hint_does_not_discard_the_segment` | One hint per type, latest wins; Bundle Length advisory |
| **Bare frames and the hook** | `tests/receiver.rs::bare_frame_through_receive_pdu`, `bundle_extent_hook_trims_padding_and_steps_over_mid_pdu_bundles`, `bundle_extent_hook_survives_its_own_panic`, `mid_pdu_bundle_without_hook_ends_the_pdu` | Delivered as `Received`; a panicking hook ends only the PDU |

### 3.15 Receiver: Per-Transfer Limits (Section 10; local policy)

*Objective: Verify the transfer cap, the bookkeeping budget, and the optional segment limit.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Transfer cap** | `tests/receiver.rs::transfer_is_rejected_as_its_data_passes_the_cap`, `oversized_bundle_message_rejected_with_event`, `transfer_at_exactly_the_cap_is_accepted_and_one_byte_over_rejected`, `bundle_of_exactly_the_cap_is_delivered_in_one_byte_segments`, `bundle_length_hint_rejects_before_accumulation` | `TooLarge` past the cap, delivered at it |
| **Bookkeeping budget** | `tests/receiver.rs::tiny_segment_flood_is_rejected_as_too_fragmented`, `retained_hint_bytes_count_against_the_bookkeeping_budget`; `receiver.rs::segments_and_hints_are_charged_to_overhead_not_data`, `malformed_bundle_length_is_charged_and_bundle_length_is_not` | `TooFragmented` past the budget |
| **Link-derived segment limit** | `tests/receiver.rs::link_derived_limit_is_four_times_the_reference_count`, `link_derived_limit_never_falls_below_64`, `link_derived_limit_of_a_pdu_no_larger_than_the_framing_assumes_one_byte_segments`, `link_derived_limit_caps_segment_data_at_the_content_length_ceiling`, `link_derived_limit_saturates_at_u32_max` | Documented formula and bounds |
| **Segment limit enforcement** | `tests/receiver.rs::small_pdu_link_delivers_a_bundle_the_per_segment_charge_rejects`, `bundle_of_exactly_the_cap_is_delivered_with_a_link_derived_limit`, `transfer_of_exactly_the_segment_limit_completes`, `segment_beyond_the_limit_rejects_the_transfer_as_too_fragmented`, `repeated_segments_do_not_consume_the_allowance`, `transfer_end_cannot_bypass_the_segment_limit`, `too_large_takes_precedence_over_too_fragmented`, `bundle_length_hint_does_not_raise_the_segment_limit`, `large_pdus_do_not_raise_the_segment_limit`, `without_a_segment_limit_the_per_segment_charge_still_applies`; `receiver.rs::segment_over_the_limit_is_counted_but_never_stored` | Limit enforced on distinct segments; the over-limit segment is not stored |

### 3.16 Receiver: Retention Limit (Section 10; local policy)

*Objective: Verify the receiver-wide bound on retained state and its sizing helper.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Sizing** | `tests/receiver.rs::retention_for_transfers_is_the_finest_segmentation_charge`, `max_retained_bytes_reports_the_limit_in_effect`, `smallest_retention_limit_admits_only_what_is_charged_nothing`, `receiver_config_sizing_table_figures`, `filled_pdus_charge_at_most_the_documented_figure`, `one_transfer_can_be_charged_exactly_for_transfers` | Matches the `ReceiverConfig` rustdoc figures |
| **Enforcement** | `tests/receiver.rs::transfer_that_would_exceed_the_retention_limit_is_rejected`, `retention_limit_below_one_transfers_allowance_is_enforced`, `retention_limit_above_one_transfers_allowance_is_enforced`, `empty_segments_count_against_the_retention_limit_under_a_segment_limit` | `ReceiverFull` for the transfer that grew |
| **Accounting** | `tests/receiver.rs::retention_is_released_by_cancel_expiry_and_rejection`, `retained_bytes_reports_the_charged_state`; `receiver.rs::retained_segment_is_charged_what_it_keeps_alive`, `duplicate_segment_is_not_charged_twice`, `retained_always_equals_the_sum_of_held_charges` | Charged total equals the sum of held charges, released on close |

### 3.17 Configuration Types

*Objective: Verify the validated configuration newtypes, their defaults, and their serde form.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Sender configuration** | `tests/sender.rs::config_defaults`, `pdu_size_boundaries`, `pdu_size_default_is_the_const`, `pdu_size_parses_and_formats_as_its_integer`, `send_queue_depth_zero_rejected`, `send_queue_depth_converts_to_and_from_non_zero`, `send_queue_depth_parses_and_formats_as_its_integer` | Range-checked; parse and format as integers |
| **Receiver configuration** | `tests/receiver.rs::config_defaults`, `max_transfer_size_zero_rejected`, `max_transfer_size_converts_to_and_from_non_zero`, `max_transfer_size_parses_and_formats_as_its_integer`, `max_segments_zero_rejected`, `max_segments_converts_to_and_from_non_zero`, `max_segments_parses_and_formats_as_its_integer`, `max_retained_bytes_zero_rejected`, `max_retained_bytes_converts_parses_and_formats_as_its_integer` | Range-checked; parse and format as integers |
| **Window size** | `tests/transfer.rs::window_size_boundaries`, `window_size_default_is_recommended`, `window_size_parses_and_formats_as_its_integer`, `window_full_names_the_window_size`, `window_reports_its_size_as_a_window_size` | Range-checked; default is the draft's recommendation |
| **Serde** (`serde`) | `tests/config.rs::sender_config_round_trips_as_kebab_case_integers`, `receiver_config_round_trips_as_kebab_case_integers`, `missing_fields_take_their_defaults`, `out_of_range_values_are_rejected_on_deserialize` | Kebab-case integers; defaults filled; out-of-range refused |

### 3.18 Tower Adapters (`tower`)

*Objective: Verify the `Service` and `Stream` adapters, their backpressure, and their wakeups.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Services** | `tests/tower.rs::receiver_service_round_trip`, `sender_service_enqueue_then_stream_drain`, `sender_service_with_layer`, `sender_service_send_request_carries_hints` | Requests reach the sender and receiver unchanged |
| **Backpressure** | `tests/tower.rs::sender_service_poll_ready_blocks_until_the_oldest_end_drains`, `sender_service_poll_ready_blocks_when_send_queue_full`, `producers_admitted_under_one_lock_hold_never_see_window_full` | `poll_ready` pending while the window or queue is full |
| **Wakeups** | `tests/tower.rs::sender_drain_wakes_pending_enqueue_task_when_window_full`, `sender_cancel_wakes_pending_enqueue_task`, `every_parked_producer_is_woken`, `cancelling_a_queued_bundle_wakes_a_producer_parked_on_queue_depth`, `draining_a_bare_frame_wakes_a_producer_parked_on_queue_depth`, `sender_enqueue_wakes_pending_drain_task` | Every capacity change wakes the parked side |
| **Stream** | `tests/tower.rs::sender_stream_pending_when_idle`, `sender_stream_yields_the_bundles_each_pdu_carries`, `stream_stays_pending_after_cancel_empties_the_queue` | Pending when idle, never finishes |

### 3.19 End to End

*Objective: Verify the sender and receiver together, including over a real socket.*

| Test Scenario | Test | Expected Output |
 | ----- | ----- | ----- |
| **Round trip** | `tests/receiver.rs::sender_receiver_round_trip`, `sender_receiver_round_trip_small` | Every bundle delivered intact |
| **Packet tunnel** | `tests/tunnel.rs::lossless_link_delivers_every_packet_in_order`, `lossy_link_delivers_exactly_the_packets_it_did_not_damage`, `udp_loopback_carries_every_packet` | Undamaged packets delivered in order; damaged transfers expire |
| **Documentation examples** | Doctests: the README example (`rand`), `codec::BundleExtent`, `receiver::ReceiverConfig` | Compile and run |

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-btpu --all-features`. The `tower`, `serde`, and `rand` scenarios need their features; `cargo test -p hardy-btpu` runs the rest.

* **`no_std` builds:** CI builds the crate for `thumbv7em-none-eabihf`, and for `thumbv6m-none-eabi` with `critical-section`.

* **Pass Criteria:** All tests listed above must return `ok`.

* **Coverage Target:** > 90% line coverage for `src/codec/`, `src/transfer.rs`, `src/sender.rs`, and `src/receiver.rs`.
