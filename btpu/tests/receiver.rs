//! Reassembly, window bookkeeping, dispositions, and PDU fault containment
//! through the public `receiver` API.

mod common;

use std::{
    num::{NonZeroU32, NonZeroUsize},
    panic::{AssertUnwindSafe, catch_unwind},
};

use bytes::{BufMut, Bytes, BytesMut};
use hardy_btpu::{
    OutOfRange, ParseError,
    codec::{
        Error as CodecError,
        header::HEADER_SIZE,
        hint::{BUNDLE_LENGTH_HINT, HintItem},
        message::{Message, TransferSegmentMessage},
    },
    fec::{ExplicitFecMessage, PreAgreedFecMessage},
    receiver::{
        DropReason, MIN_OVERHEAD_BUDGET, MaxBundleSize, MaxRetainedBytes, MaxSegments, Receiver,
        ReceiverConfig, ReceiverEvent, RejectReason, SEGMENT_OVERHEAD,
    },
    sender::{SendOptions, Sender},
};

use self::common::{
    bpv7_like, encode, end, receiver, receiver_with_max_segments, segment, sender, window_size,
};

fn default_receiver() -> Receiver {
    Receiver::new(ReceiverConfig::default())
}

fn received(data: &'static [u8]) -> ReceiverEvent {
    ReceiverEvent::BundleReceived {
        data: Bytes::from_static(data),
        hints: vec![],
    }
}

fn dropped(transfer_number: u32, reason: DropReason) -> ReceiverEvent {
    ReceiverEvent::MessageDropped {
        transfer_number,
        reason,
    }
}

fn rejected(transfer_number: u32, reason: RejectReason) -> ReceiverEvent {
    ReceiverEvent::TransferRejected {
        transfer_number,
        reason,
    }
}

// Whether `inner` is a view into the allocation `outer` was decoded from.
fn is_within(inner: &Bytes, outer: &Bytes) -> bool {
    let start = outer.as_ptr() as usize;
    let p = inner.as_ptr() as usize;
    p >= start && p + inner.len() <= start + outer.len()
}

#[test]
fn config_defaults() {
    let c = ReceiverConfig::default();
    assert_eq!(c.window_size.get(), 16);
    assert_eq!(c.max_bundle_size.get(), 0x4000_0000);
    assert_eq!(c.max_segments_per_transfer, None);
    assert!(!c.fec);
}

#[test]
fn max_bundle_size_zero_rejected() {
    assert_eq!(MaxBundleSize::new(0), None);
    assert_eq!(
        MaxBundleSize::try_from(0),
        Err(OutOfRange {
            name: "max bundle size",
            value: 0,
            min: 1,
            max: None,
        })
    );
    assert_eq!(MaxBundleSize::try_from(1), Ok(MaxBundleSize::MIN));
    assert_eq!(MaxBundleSize::try_from(usize::MAX), Ok(MaxBundleSize::MAX));
}

#[test]
fn max_bundle_size_converts_to_and_from_non_zero() {
    let n = NonZeroUsize::new(4096).unwrap();
    let cap = MaxBundleSize::from(n);
    assert_eq!(cap.get(), 4096);
    assert_eq!(NonZeroUsize::from(cap), n);
    assert_eq!(usize::from(cap), 4096);
    assert_eq!(cap.to_string(), "4096");
    assert_eq!(MaxBundleSize::default(), MaxBundleSize::DEFAULT);
}

#[test]
fn max_bundle_size_parses_and_formats_as_its_integer() {
    assert_eq!("1073741824".parse(), Ok(MaxBundleSize::DEFAULT));
    assert_eq!(
        "0".parse::<MaxBundleSize>(),
        Err(ParseError::OutOfRange(
            MaxBundleSize::try_from(0).unwrap_err()
        ))
    );
    assert_eq!(
        "1 GiB".parse::<MaxBundleSize>(),
        Err(ParseError::Syntax {
            name: "max bundle size",
            source: "1 GiB".parse::<usize>().unwrap_err(),
        })
    );
    let m = MaxBundleSize::DEFAULT;
    assert_eq!(format!("{m:#x} {m:#o}"), "0x40000000 0o10000000000");
}

#[test]
fn bundle_message_immediate() {
    let mut r = default_receiver();
    let events = r.process_message(Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"hello"),
    });
    assert_eq!(events, vec![received(b"hello")]);
}

#[test]
fn two_segment_transfer() {
    let mut r = default_receiver();
    assert_eq!(r.process_message(segment(0, 0, b"hel")), vec![]);
    assert_eq!(
        r.process_message(end(0, 1, b"lo")),
        vec![received(b"hello")]
    );
}

#[test]
fn out_of_order_completes_on_end_recheck() {
    let mut r = default_receiver();
    assert_eq!(r.process_message(segment(0, 1, b"ld")), vec![]);
    assert_eq!(r.process_message(segment(0, 0, b"wor")), vec![]);
    assert_eq!(
        r.process_message(end(0, 2, b"!")),
        vec![received(b"world!")]
    );
}

#[test]
fn end_before_late_segment_completes_on_the_segment() {
    let mut r = default_receiver();
    assert_eq!(r.process_message(end(0, 1, b"ld")), vec![]);
    // The late segment 0 fills the final gap; completion fires here, on
    // the segment insert, even though the End arrived earlier.
    assert_eq!(
        r.process_message(segment(0, 0, b"wor")),
        vec![received(b"world")]
    );
}

#[test]
fn conflicting_end_dropped_and_transfer_still_completes() {
    let mut r = default_receiver();
    r.process_message(segment(0, 0, b"a"));
    r.process_message(end(0, 2, b"c"));

    // A second End disagreeing with the established final index is
    // dropped; accepting it would wedge completion forever.
    assert_eq!(
        r.process_message(end(0, 1, b"y")),
        vec![dropped(0, DropReason::SegmentIndexConflict)]
    );

    // The established index (and not the bogus End's data) still stands:
    // the missing middle segment completes the transfer.
    assert_eq!(
        r.process_message(segment(0, 1, b"b")),
        vec![received(b"abc")]
    );
}

#[test]
fn segment_beyond_final_index_dropped() {
    let mut r = default_receiver();
    r.process_message(segment(0, 0, b"a"));
    r.process_message(end(0, 2, b"c"));

    // A stray segment above the established final index is dropped;
    // storing it would keep the map's highest key above N forever.
    assert_eq!(
        r.process_message(segment(0, 7, b"x")),
        vec![dropped(0, DropReason::SegmentIndexConflict)]
    );
    assert_eq!(
        r.process_message(segment(0, 1, b"b")),
        vec![received(b"abc")]
    );
}

#[test]
fn end_below_seen_segment_dropped() {
    let mut r = default_receiver();
    r.process_message(segment(0, 0, b"a"));
    r.process_message(segment(0, 1, b"b"));
    r.process_message(segment(0, 2, b"c"));

    // An End claiming a final index below a segment already seen is
    // bogus (segments beyond the final index cannot exist); it must not
    // record a final index the map can never satisfy.
    assert_eq!(
        r.process_message(end(0, 1, b"y")),
        vec![dropped(0, DropReason::SegmentIndexConflict)]
    );
    // The genuine End (a repeat of segment 2's data) completes the transfer.
    assert_eq!(r.process_message(end(0, 2, b"c")), vec![received(b"abc")]);
}

#[test]
fn duplicate_segments_ignored() {
    let mut r = default_receiver();
    r.process_message(segment(0, 0, b"abc"));
    // Section 6 permits duplicates; the first copy wins.
    assert_eq!(
        r.process_message(segment(0, 0, b"SHOULD BE IGNORED")),
        vec![]
    );
    assert_eq!(
        r.process_message(end(0, 1, b"def")),
        vec![received(b"abcdef")]
    );
}

#[test]
fn empty_end_completes_the_transfer() {
    // Section 4: the transfer is complete once segments 0..=N are present.
    // A streaming sender that only learns the input ended after emitting a
    // full segment finishes with an empty End at N (Section 8.3 says it
    // SHOULD NOT, not that a receiver may refuse it).
    let mut r = default_receiver();
    r.process_message(segment(0, 0, b"ab"));
    assert_eq!(r.process_message(end(0, 1, b"")), vec![received(b"ab")]);
}

#[test]
fn empty_middle_segment_counts_toward_completion() {
    let mut r = default_receiver();
    r.process_message(segment(0, 0, b"a"));
    assert_eq!(r.process_message(segment(0, 1, b"")), vec![]);
    assert_eq!(r.process_message(end(0, 2, b"c")), vec![received(b"ac")]);
}

#[test]
fn repeated_cancel_is_idempotent() {
    let mut r = default_receiver();
    r.process_message(segment(5, 0, b"data"));
    assert_eq!(
        r.process_message(Message::TransferCancel { transfer_number: 5 }),
        vec![ReceiverEvent::TransferCancelled { transfer_number: 5 }]
    );
    // A repeated Cancel is reported as a drop, not a second cancellation.
    assert_eq!(
        r.process_message(Message::TransferCancel { transfer_number: 5 }),
        vec![dropped(5, DropReason::Cancelled)]
    );
    // And a late End must not deliver a bundle.
    assert_eq!(
        r.process_message(end(5, 1, b"tail")),
        vec![dropped(5, DropReason::Cancelled)]
    );
}

#[test]
fn cancel_of_unknown_transfer_ignored() {
    let mut r = default_receiver();
    r.process_message(segment(0, 0, b"hel"));

    // Section 8.4: Cancel for a never-seen transfer number is ignored:
    // no TransferCancelled, and no window advance (a large number here
    // would otherwise expire transfer 0).
    assert_eq!(
        r.process_message(Message::TransferCancel {
            transfer_number: 1000,
        }),
        vec![dropped(1000, DropReason::UnknownTransfer)]
    );
    assert_eq!(
        r.process_message(end(0, 1, b"lo")),
        vec![received(b"hello")]
    );
}

#[test]
fn cancel_of_an_in_window_transfer_before_its_segments_is_remembered() {
    // Section 8.4: "prior or later received Segments ... MUST be
    // discarded", and Section 5 makes every number in the window an
    // in-progress transfer.  A Cancel reordered ahead of the segments it
    // cancels (or repeated after them) must therefore stick.
    let mut r = default_receiver();
    r.process_message(segment(5, 0, b"other"));
    assert_eq!(
        r.process_message(Message::TransferCancel { transfer_number: 3 }),
        vec![ReceiverEvent::TransferCancelled { transfer_number: 3 }]
    );
    assert_eq!(
        r.process_message(segment(3, 0, b"hel")),
        vec![dropped(3, DropReason::Cancelled)]
    );
    assert_eq!(
        r.process_message(end(3, 1, b"lo")),
        vec![dropped(3, DropReason::Cancelled)]
    );
}

#[test]
fn repeated_messages_of_a_delivered_transfer_do_not_redeliver() {
    // Section 6 lets a sender repeat any message.  A repeat of a completed
    // transfer must neither deliver the bundle twice nor leave a phantom
    // transfer behind to expire later.
    let mut r = receiver(4, usize::MAX);
    let mut pdu = BytesMut::new();
    put_completing_transfer(&mut pdu);
    let pdu = pdu.freeze();
    assert_eq!(r.receive_pdu(pdu.clone()), vec![received(b"hello")]);
    assert_eq!(
        r.receive_pdu(pdu),
        vec![
            dropped(0, DropReason::Delivered),
            dropped(0, DropReason::Delivered)
        ]
    );
    // A lone repeated segment opens nothing: when the window moves past
    // transfer 0 there is nothing to expire.
    assert_eq!(
        r.process_message(segment(0, 0, b"hel")),
        vec![dropped(0, DropReason::Delivered)]
    );
    for t in 1..=4u32 {
        assert_eq!(r.process_message(segment(t, 0, b"x")), vec![]);
    }
}

#[test]
fn outside_window_drop_reported() {
    let mut r = receiver(4, usize::MAX);
    for t in 0..8u32 {
        r.process_message(segment(t, 0, b"x"));
    }
    // greatest = 7, window = 4: transfer 0 is well outside.
    assert_eq!(
        r.process_message(segment(0, 1, b"y")),
        vec![dropped(0, DropReason::OutsideWindow)]
    );
}

#[test]
fn window_wraparound_with_live_transfers() {
    let mut r = receiver(4, usize::MAX);
    let first = u32::MAX - 1;
    for t in [first, u32::MAX, 0, 1] {
        assert_eq!(r.process_message(segment(t, 0, b"x")), vec![]);
    }
    // greatest = 1; MAX-1 is 3 behind it modulo 2^32, still in window.
    assert_eq!(
        r.process_message(end(first, 1, b"y")),
        vec![received(b"xy")]
    );
    // 2 pushes nothing out (MAX is 3 behind); 3 expires MAX exactly.
    assert_eq!(r.process_message(segment(2, 0, b"x")), vec![]);
    assert_eq!(
        r.process_message(segment(3, 0, b"x")),
        vec![ReceiverEvent::TransferExpired {
            transfer_number: u32::MAX
        }]
    );
}

#[test]
fn transfers_straddling_the_wrap_expire_oldest_first() {
    let mut r = receiver(4, usize::MAX);
    for t in [u32::MAX - 1, u32::MAX, 0] {
        assert_eq!(r.process_message(segment(t, 0, b"x")), vec![]);
    }
    // Numeric order would report 0 first; window order puts it last.
    assert_eq!(
        r.process_message(segment(5, 0, b"x")),
        vec![
            ReceiverEvent::TransferExpired {
                transfer_number: u32::MAX - 1
            },
            ReceiverEvent::TransferExpired {
                transfer_number: u32::MAX
            },
            ReceiverEvent::TransferExpired { transfer_number: 0 },
        ]
    );
}

#[test]
fn number_behind_a_just_wrapped_greatest_expires_before_it() {
    let mut r = receiver(4, usize::MAX);
    assert_eq!(r.process_message(segment(0, 0, b"x")), vec![]);
    // Both are behind 0, not far ahead of it.
    assert_eq!(r.process_message(segment(u32::MAX - 2, 0, b"x")), vec![]);
    assert_eq!(r.process_message(segment(u32::MAX, 0, b"x")), vec![]);

    assert_eq!(
        r.process_message(segment(1, 0, b"x")),
        vec![ReceiverEvent::TransferExpired {
            transfer_number: u32::MAX - 2
        }]
    );
    assert_eq!(
        r.process_message(segment(4, 0, b"x")),
        vec![
            ReceiverEvent::TransferExpired {
                transfer_number: u32::MAX
            },
            ReceiverEvent::TransferExpired { transfer_number: 0 },
        ]
    );
}

#[test]
fn number_more_than_half_the_space_ahead_advances_the_window() {
    // Section 5 treats up to 2^31 + W/2 - 1 ahead as new, past the half of
    // the space where serial-number comparison would call it behind.
    let mut r = receiver(4, usize::MAX);
    assert_eq!(r.process_message(segment(0, 0, b"x")), vec![]);
    let far = (1 << 31) + 1;
    assert_eq!(
        r.process_message(segment(far, 0, b"x")),
        vec![ReceiverEvent::TransferExpired { transfer_number: 0 }]
    );
    // The window now trails `far`: a number just behind it is open, and the
    // next new one expires them oldest first.
    assert_eq!(r.process_message(segment(far - 3, 0, b"x")), vec![]);
    assert_eq!(
        r.process_message(segment(far + 1, 0, b"x")),
        vec![ReceiverEvent::TransferExpired {
            transfer_number: far - 3
        }]
    );
    assert_eq!(
        r.process_message(segment(far + 5, 0, b"x")),
        vec![
            ReceiverEvent::TransferExpired {
                transfer_number: far
            },
            ReceiverEvent::TransferExpired {
                transfer_number: far + 1
            },
        ]
    );
}

#[test]
fn reset_forgets_delivered_transfers() {
    let mut r = default_receiver();
    assert_eq!(r.process_message(end(3, 0, b"x")), vec![received(b"x")]);
    assert_eq!(
        r.process_message(end(3, 0, b"x")),
        vec![dropped(3, DropReason::Delivered)]
    );

    // After a reset the same number is a new transfer from a new sender.
    r.reset();
    assert_eq!(r.process_message(end(3, 0, b"x")), vec![received(b"x")]);
}

#[test]
fn reset_accepts_a_restarted_sender() {
    let mut r = receiver(4, usize::MAX);
    for t in 1000..1004u32 {
        r.process_message(segment(t, 0, b"x"));
    }
    // A peer that restarted from a low number is outside the window...
    assert_eq!(
        r.process_message(segment(3, 0, b"he")),
        vec![dropped(3, DropReason::OutsideWindow)]
    );
    // ...until the CLA, told of the restart, resets the receiver.
    r.reset();
    assert_eq!(r.process_message(segment(3, 0, b"he")), vec![]);
    assert_eq!(r.process_message(end(3, 1, b"y")), vec![received(b"hey")]);
}

// A pre-agreed FEC Source message for `transfer_number`.
fn pre_agreed_fec(transfer_number: u32, fec_instance_id: u8) -> Message {
    Message::PreAgreedFecSource(PreAgreedFecMessage {
        transfer_number,
        fec_instance_id,
        hints: vec![],
        payload: Bytes::from_static(b"fec"),
    })
}

// An explicit FEC Repair message for `transfer_number`.
fn explicit_fec(transfer_number: u32, fec_encoding_id: u8) -> Message {
    Message::ExplicitFecRepair(ExplicitFecMessage {
        transfer_number,
        fec_encoding_id,
        hints: vec![],
        payload: Bytes::from_static(b"fec"),
    })
}

fn fec_receiver() -> Receiver {
    Receiver::new(ReceiverConfig {
        fec: true,
        ..ReceiverConfig::default()
    })
}

#[test]
fn fec_transfer_expires_like_a_core_one() {
    let mut r = fec_receiver();
    assert_eq!(r.process_message(pre_agreed_fec(0, 1)), vec![]);
    assert_eq!(
        r.process_message(segment(16, 0, b"x")),
        vec![ReceiverEvent::TransferExpired { transfer_number: 0 }]
    );
}

#[test]
fn fec_message_on_core_transfer_rejects_it() {
    // FEC Section 3.2: mixing MUST cancel the transfer, so the core
    // transfer can no longer complete.
    let mut r = default_receiver();
    r.process_message(segment(0, 0, b"hel"));
    assert_eq!(
        r.process_message(pre_agreed_fec(0, 1)),
        vec![rejected(0, RejectReason::FecCoreMixing)]
    );
    assert_eq!(
        r.process_message(end(0, 1, b"lo")),
        vec![dropped(
            0,
            DropReason::Rejected(RejectReason::FecCoreMixing)
        )]
    );
}

#[test]
fn core_message_on_fec_transfer_rejects_it() {
    let mut r = default_receiver();
    assert_eq!(r.process_message(pre_agreed_fec(0, 1)), vec![]);
    assert_eq!(
        r.process_message(segment(0, 0, b"x")),
        vec![rejected(0, RejectReason::FecCoreMixing)]
    );
    assert_eq!(
        r.process_message(pre_agreed_fec(0, 1)),
        vec![dropped(
            0,
            DropReason::Rejected(RejectReason::FecCoreMixing)
        )]
    );
}

#[test]
fn core_fec_mixing_rejects_through_receive_pdu() {
    // Both directions through the decoder, with FEC decoding enabled.
    let mut r = fec_receiver();
    assert_eq!(r.receive_pdu(encode(&segment(0, 0, b"abc"))), vec![]);
    assert_eq!(
        r.receive_pdu(encode(&pre_agreed_fec(0, 1))),
        vec![rejected(0, RejectReason::FecCoreMixing)]
    );
    assert_eq!(
        r.receive_pdu(encode(&end(0, 1, b"def"))),
        vec![dropped(
            0,
            DropReason::Rejected(RejectReason::FecCoreMixing)
        )]
    );

    assert_eq!(r.receive_pdu(encode(&explicit_fec(1, 6))), vec![]);
    assert_eq!(
        r.receive_pdu(encode(&end(1, 0, b"x"))),
        vec![rejected(1, RejectReason::FecCoreMixing)]
    );
    assert_eq!(
        r.receive_pdu(encode(&explicit_fec(1, 6))),
        vec![dropped(
            1,
            DropReason::Rejected(RejectReason::FecCoreMixing)
        )]
    );
}

#[test]
fn fec_messages_with_one_configuration_keep_the_transfer_open() {
    let mut r = fec_receiver();
    let repair = Message::PreAgreedFecRepair(PreAgreedFecMessage {
        transfer_number: 0,
        fec_instance_id: 1,
        hints: vec![],
        payload: Bytes::from_static(b"repair"),
    });
    assert_eq!(r.receive_pdu(encode(&pre_agreed_fec(0, 1))), vec![]);
    assert_eq!(r.receive_pdu(encode(&repair)), vec![]);
    assert_eq!(r.receive_pdu(encode(&pre_agreed_fec(0, 1))), vec![]);
}

#[test]
fn changed_fec_instance_id_rejects_the_transfer() {
    // FEC Section 3.1: a change of FEC Instance ID MUST cancel the
    // transfer, and the original ID cannot revive it.
    let mut r = fec_receiver();
    assert_eq!(r.receive_pdu(encode(&pre_agreed_fec(0, 1))), vec![]);
    assert_eq!(
        r.receive_pdu(encode(&pre_agreed_fec(0, 2))),
        vec![rejected(0, RejectReason::FecConfigurationChanged)]
    );
    assert_eq!(
        r.receive_pdu(encode(&pre_agreed_fec(0, 1))),
        vec![dropped(
            0,
            DropReason::Rejected(RejectReason::FecConfigurationChanged)
        )]
    );
}

#[test]
fn changed_fec_encoding_id_rejects_the_transfer() {
    // FEC Section 3: the FEC Framework Configuration Information MUST NOT
    // change mid-transfer; the Encoding ID is the part visible without a
    // scheme.
    let mut r = fec_receiver();
    assert_eq!(r.receive_pdu(encode(&explicit_fec(0, 6))), vec![]);
    assert_eq!(
        r.receive_pdu(encode(&explicit_fec(0, 5))),
        vec![rejected(0, RejectReason::FecConfigurationChanged)]
    );
}

#[test]
fn switching_between_pre_agreed_and_explicit_fec_rejects_the_transfer() {
    // An Instance ID and an Encoding ID name different things, so the
    // switch is a change even when the two values are equal.
    let mut r = fec_receiver();
    assert_eq!(r.receive_pdu(encode(&pre_agreed_fec(0, 1))), vec![]);
    assert_eq!(
        r.receive_pdu(encode(&explicit_fec(0, 1))),
        vec![rejected(0, RejectReason::FecConfigurationChanged)]
    );

    assert_eq!(r.receive_pdu(encode(&explicit_fec(1, 1))), vec![]);
    assert_eq!(
        r.receive_pdu(encode(&pre_agreed_fec(1, 1))),
        vec![rejected(1, RejectReason::FecConfigurationChanged)]
    );
}

#[test]
fn fec_types_do_not_touch_the_window_unless_enabled() {
    // 0x70..=0x73 are Private Use.  With FEC decoding off, a peer's
    // private message of type 0x70 naming a far-off transfer number must
    // not open an FEC transfer and expire the live core transfer.
    let mut r = receiver(4, usize::MAX);
    r.process_message(segment(0, 0, b"hel"));
    let private = Bytes::from_static(&[0x70, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x2A, 0x01]);
    assert_eq!(r.receive_pdu(private.clone()), vec![]);
    assert_eq!(
        r.process_message(end(0, 1, b"lo")),
        vec![received(b"hello")]
    );

    // The same PDU with FEC enabled opens transfer 42 and expires 0.
    let mut r = Receiver::new(ReceiverConfig {
        fec: true,
        ..ReceiverConfig::default()
    });
    r.process_message(segment(0, 0, b"hel"));
    let mut far = BytesMut::from(private.as_ref());
    far[4..8].copy_from_slice(&1000u32.to_be_bytes());
    assert_eq!(
        r.receive_pdu(far.freeze()),
        vec![ReceiverEvent::TransferExpired { transfer_number: 0 }]
    );
}

#[test]
fn fec_transfer_promising_an_oversized_bundle_is_rejected() {
    let mut r = Receiver::new(ReceiverConfig {
        fec: true,
        max_bundle_size: MaxBundleSize::try_from(5).unwrap(),
        ..ReceiverConfig::default()
    });
    let pdu = encode(&Message::PreAgreedFecSource(PreAgreedFecMessage {
        transfer_number: 9,
        fec_instance_id: 1,
        hints: vec![HintItem::BundleLength(1 << 40)],
        payload: Bytes::from_static(b"fec"),
    }));
    assert_eq!(
        r.receive_pdu(pdu),
        vec![ReceiverEvent::TransferRejected {
            transfer_number: 9,
            reason: RejectReason::TooLarge,
        }]
    );
}

// Encode a two-segment transfer of b"hello" into `pdu`.
fn put_completing_transfer(pdu: &mut BytesMut) {
    pdu.put_slice(&encode(&segment(0, 0, b"hel")));
    pdu.put_slice(&encode(&end(0, 1, b"lo")));
}

#[test]
fn malformed_message_mid_pdu_keeps_prior_events_and_continues() {
    let mut r = default_receiver();

    // One PDU: a transfer that completes, then a known-type message with
    // a malformed interior, then a well-formed Bundle.
    let mut pdu = BytesMut::new();
    put_completing_transfer(&mut pdu);
    // Bundle message, H flag set, content = malformed hint chain (a hint
    // header promising a 255-byte value with no bytes behind it).
    pdu.put_slice(&[0x02, 0x80, 0x00, 0x02, 0x1F, 0xFF]);
    pdu.put_slice(&encode(&Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"ok"),
    }));

    // The completed transfer's bundle survives the later fault, the bad
    // message is reported and skipped, and the trailing Bundle is still
    // processed via the next header-length boundary.
    assert_eq!(
        r.receive_pdu(pdu.freeze()),
        vec![
            received(b"hello"),
            ReceiverEvent::MalformedMessage {
                error: CodecError::InsufficientData {
                    needed: 257,
                    available: 2,
                },
            },
            received(b"ok"),
        ]
    );
}

#[test]
fn malformed_pdu_keeps_prior_events() {
    let mut r = default_receiver();
    // A completing transfer followed by a truncated header: the trailing
    // bytes are undecodable (no message boundary), but the bundle
    // completed earlier in the PDU must still be delivered alongside the
    // MalformedPdu report.
    let mut pdu = BytesMut::new();
    put_completing_transfer(&mut pdu);
    let header_at = pdu.len();
    pdu.put_slice(&[0x02, 0x30]); // 2 bytes: too short for a 4-byte header

    // Counted from the start of the PDU.
    assert_eq!(
        r.receive_pdu(pdu.freeze()),
        vec![
            received(b"hello"),
            ReceiverEvent::MalformedPdu {
                error: CodecError::InsufficientData {
                    needed: header_at + HEADER_SIZE,
                    available: header_at + 2,
                },
            },
        ]
    );
}

#[test]
fn oversized_bundle_message_rejected_with_event() {
    let mut r = receiver(16, 5);
    let data = Bytes::from_static(b"too long bundle");
    let len = data.len();
    assert_eq!(
        r.process_message(Message::Bundle {
            hints: vec![],
            data,
        }),
        vec![ReceiverEvent::BundleRejected { len }]
    );
    // Exactly at the limit is accepted.
    assert_eq!(
        r.process_message(Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"12345"),
        }),
        vec![received(b"12345")]
    );
}

#[test]
fn empty_bundle_message_rejected() {
    // Section 8.1: the content MUST be a valid bundle, which zero bytes
    // cannot be.
    let mut r = default_receiver();
    assert_eq!(
        r.process_message(Message::Bundle {
            hints: vec![],
            data: Bytes::new(),
        }),
        vec![ReceiverEvent::BundleRejected { len: 0 }]
    );
}

#[test]
fn transfer_with_no_data_is_rejected() {
    // The Section 8.1 policy above applies to a reassembled bundle: every
    // segment may be empty individually, but not all of them.
    let mut r = default_receiver();
    assert_eq!(r.process_message(segment(0, 0, b"")), vec![]);
    assert_eq!(
        r.process_message(end(0, 1, b"")),
        vec![rejected(0, RejectReason::EmptyBundle)]
    );
    assert_eq!(
        r.process_message(end(0, 1, b"")),
        vec![dropped(0, DropReason::Rejected(RejectReason::EmptyBundle))]
    );
}

#[test]
fn empty_single_end_is_rejected_through_receive_pdu() {
    let mut r = default_receiver();
    assert_eq!(
        r.receive_pdu(encode(&end(0, 0, b""))),
        vec![rejected(0, RejectReason::EmptyBundle)]
    );
}

#[test]
fn bundle_length_hint_on_bundle_message_is_ignored() {
    // Section 9.1: receivers SHOULD ignore the hint outside Segment/End
    // messages; other hints on the Bundle message still surface.
    let mut r = default_receiver();
    let other = HintItem::Unknown {
        hint_type: 0x41,
        value: Bytes::from_static(b"z"),
    };
    assert_eq!(
        r.process_message(Message::Bundle {
            hints: vec![HintItem::BundleLength(2), other.clone()],
            data: Bytes::from_static(b"hi"),
        }),
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from_static(b"hi"),
            hints: vec![other],
        }]
    );
}

#[test]
fn transfer_at_exactly_the_cap_is_accepted_and_one_byte_over_rejected() {
    let exact = 6;
    let mut r = receiver(16, exact);
    r.process_message(Message::TransferSegment(TransferSegmentMessage {
        transfer_number: 0,
        segment_index: 0,
        hints: vec![HintItem::BundleLength(6)],
        data: Bytes::from_static(b"abc"),
    }));
    assert_eq!(
        r.process_message(end(0, 1, b"def")),
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from_static(b"abcdef"),
            hints: vec![HintItem::BundleLength(6)],
        }]
    );

    let mut r = receiver(16, exact - 1);
    r.process_message(segment(0, 0, b"abc"));
    assert_eq!(
        r.process_message(end(0, 1, b"def")),
        vec![ReceiverEvent::TransferRejected {
            transfer_number: 0,
            reason: RejectReason::TooLarge,
        }]
    );
}

#[test]
fn bundle_of_exactly_the_cap_is_delivered_in_one_byte_segments() {
    // The cap is a policy on the bundle's length: per-segment bookkeeping
    // is budgeted separately, and six segments are well within the
    // bookkeeping floor, so a cap-sized bundle in one-byte segments is not
    // pushed over.
    let mut r = receiver(16, 6);
    for i in 0..5u32 {
        assert_eq!(r.process_message(segment(0, i, b"x")), vec![]);
    }
    assert_eq!(
        r.process_message(end(0, 5, b"x")),
        vec![received(b"xxxxxx")]
    );

    // One byte more, and the transfer is rejected the moment the cap is
    // exceeded, before its End.
    let mut r = receiver(16, 6);
    for i in 0..6u32 {
        assert_eq!(r.process_message(segment(1, i, b"x")), vec![]);
    }
    assert_eq!(
        r.process_message(segment(1, 6, b"x")),
        vec![ReceiverEvent::TransferRejected {
            transfer_number: 1,
            reason: RejectReason::TooLarge,
        }]
    );
}

#[test]
fn tiny_segment_flood_is_rejected_as_too_fragmented() {
    // Empty segments carry no bytes the cap could see, so only the
    // bookkeeping budget bounds them: with a cap below the floor, the
    // floor's worth of segments and not one more.
    let mut r = receiver(16, 1);
    let budget = MIN_OVERHEAD_BUDGET / SEGMENT_OVERHEAD;
    for i in 0..budget {
        assert_eq!(r.process_message(segment(0, i as u32, b"")), vec![]);
    }
    assert_eq!(
        r.process_message(segment(0, budget as u32, b"")),
        vec![ReceiverEvent::TransferRejected {
            transfer_number: 0,
            reason: RejectReason::TooFragmented,
        }]
    );
    // And it stays rejected.
    assert_eq!(
        r.process_message(end(0, budget as u32 + 1, b"")),
        vec![dropped(
            0,
            DropReason::Rejected(RejectReason::TooFragmented)
        )]
    );
}

#[test]
fn retained_hint_bytes_count_against_the_bookkeeping_budget() {
    // Sixteen distinct 255-byte hints are 16 × 257 encoded bytes, which
    // with one segment's overhead exceeds the 4 KiB floor.
    let mut r = receiver(16, 1);
    let hints: Vec<HintItem> = (0x40..0x50u8)
        .map(|hint_type| HintItem::Unknown {
            hint_type,
            value: Bytes::from(vec![hint_type; 255]),
        })
        .collect();
    assert_eq!(
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: hints.clone(),
            data: Bytes::from_static(b"a"),
        })),
        vec![ReceiverEvent::TransferRejected {
            transfer_number: 0,
            reason: RejectReason::TooFragmented,
        }]
    );

    // The same hints on a transfer with a cap to match are fine.
    let mut r = receiver(16, 16 * 257 + SEGMENT_OVERHEAD);
    assert_eq!(
        r.process_message(Message::TransferEnd(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: hints.clone(),
            data: Bytes::from_static(b"a"),
        })),
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from_static(b"a"),
            hints,
        }]
    );
}

#[test]
fn bundle_length_hint_rejects_before_accumulation() {
    let mut r = receiver(16, 5);
    assert_eq!(
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![HintItem::BundleLength(100)],
            data: Bytes::from_static(b"a"),
        })),
        vec![ReceiverEvent::TransferRejected {
            transfer_number: 0,
            reason: RejectReason::TooLarge,
        }]
    );
    // A further segment must not re-create the rejected transfer.
    assert_eq!(
        r.process_message(segment(0, 1, b"x")),
        vec![dropped(0, DropReason::Rejected(RejectReason::TooLarge))]
    );
}

#[test]
fn bundle_length_hint_smaller_than_the_data_still_delivers() {
    // The hint is advisory and only ever used to reject early; a sender
    // that under-reports is not second-guessed.
    let mut r = default_receiver();
    r.process_message(Message::TransferSegment(TransferSegmentMessage {
        transfer_number: 0,
        segment_index: 0,
        hints: vec![HintItem::BundleLength(2)],
        data: Bytes::from_static(b"abc"),
    }));
    assert_eq!(
        r.process_message(end(0, 1, b"def")),
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from_static(b"abcdef"),
            hints: vec![HintItem::BundleLength(2)],
        }]
    );
}

#[test]
fn malformed_bundle_length_hint_does_not_discard_the_segment() {
    let mut r = default_receiver();
    let mut pdu = BytesMut::new();
    pdu.put_slice(&[0x03, 0x80, 0x00, 2 + 3 + 8 + 3]);
    pdu.put_slice(&[BUNDLE_LENGTH_HINT << 1, 3, 1, 2, 3]);
    pdu.put_u32(0);
    pdu.put_u32(0);
    pdu.put_slice(b"abc");
    assert_eq!(r.receive_pdu(pdu.freeze()), vec![]);
    assert_eq!(
        r.process_message(end(0, 1, b"d")),
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from_static(b"abcd"),
            hints: vec![HintItem::Unknown {
                hint_type: BUNDLE_LENGTH_HINT,
                value: Bytes::from_static(&[1, 2, 3]),
            }],
        }]
    );
}

#[test]
fn single_segment_transfer_shares_the_segment_bytes() {
    let mut r = default_receiver();
    let payload = Bytes::from_static(b"solo bundle");
    let events = r.process_message(Message::TransferEnd(TransferSegmentMessage {
        transfer_number: 0,
        segment_index: 0,
        hints: vec![],
        data: payload.clone(),
    }));
    // Delivered without copying: the event's Bytes views the same
    // allocation as the received segment (a refcount bump, not a
    // rebuild).
    assert_eq!(events, vec![received(b"solo bundle")]);
    let ReceiverEvent::BundleReceived { data, .. } = &events[0] else {
        unreachable!()
    };
    assert_eq!(data.as_ptr(), payload.as_ptr());
}

#[test]
fn segments_shorter_than_half_the_pdu_are_copied_out_of_it() {
    // A 4-byte segment in a 64-byte PDU would pin all 64 bytes for the
    // life of the transfer; it is copied instead.
    let mut r = default_receiver();
    let mut pdu = BytesMut::from(encode(&end(0, 0, b"solo")).as_ref());
    pdu.resize(64, 0);
    let pdu = pdu.freeze();
    let events = r.receive_pdu(pdu.clone());
    let [ReceiverEvent::BundleReceived { data, .. }] = events.as_slice() else {
        panic!("expected one BundleReceived, got {events:?}");
    };
    assert_eq!(data.as_ref(), b"solo");
    assert!(!is_within(data, &pdu));

    // A segment of at least half the PDU stays a view into it.
    let mut r = default_receiver();
    let pdu = encode(&end(0, 0, b"twelve bytes")); // 12 + 12 = 24-byte PDU
    let events = r.receive_pdu(pdu.clone());
    let [ReceiverEvent::BundleReceived { data, .. }] = events.as_slice() else {
        panic!("expected one BundleReceived, got {events:?}");
    };
    assert!(is_within(data, &pdu));
}

#[test]
fn retained_hint_values_are_copied_out_of_the_pdu() {
    let mut r = default_receiver();
    let mut pdu = BytesMut::from(
        encode(&Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![HintItem::Unknown {
                hint_type: 0x41,
                value: Bytes::from_static(b"corr"),
            }],
            data: Bytes::from_static(b"hel"),
        }))
        .as_ref(),
    );
    pdu.resize(64, 0);
    let pdu = pdu.freeze();
    assert_eq!(r.receive_pdu(pdu.clone()), vec![]);
    let events = r.process_message(end(0, 1, b"lo"));
    let [ReceiverEvent::BundleReceived { hints, .. }] = events.as_slice() else {
        panic!("expected one BundleReceived, got {events:?}");
    };
    let [HintItem::Unknown { value, .. }] = hints.as_slice() else {
        panic!("expected the correlator hint, got {hints:?}");
    };
    assert_eq!(value.as_ref(), b"corr");
    assert!(!is_within(value, &pdu));
}

#[test]
fn bundle_received_surfaces_transfer_hints() {
    let mut r = default_receiver();
    let correlator_v1 = HintItem::Unknown {
        hint_type: 0x41,
        value: Bytes::from_static(b"\x07"),
    };
    let correlator_v2 = HintItem::Unknown {
        hint_type: 0x41,
        value: Bytes::from_static(b"\x09"),
    };
    r.process_message(Message::TransferSegment(TransferSegmentMessage {
        transfer_number: 0,
        segment_index: 0,
        hints: vec![HintItem::BundleLength(5), correlator_v1],
        data: Bytes::from_static(b"hel"),
    }));
    // A later message repeating the hint type supersedes the value.
    assert_eq!(
        r.process_message(Message::TransferEnd(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![correlator_v2.clone()],
            data: Bytes::from_static(b"lo"),
        })),
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from_static(b"hello"),
            hints: vec![HintItem::BundleLength(5), correlator_v2],
        }]
    );
}

#[test]
fn bundle_message_hints_deduped_latest_wins() {
    // The Bundle message path honours the same BundleReceived contract as
    // the transfer path: one item per hint type, latest wins, ordered by
    // type.
    let mut r = default_receiver();
    let stale = HintItem::Unknown {
        hint_type: 0x41,
        value: Bytes::from_static(b"old"),
    };
    let fresh = HintItem::Unknown {
        hint_type: 0x41,
        value: Bytes::from_static(b"new"),
    };
    let other = HintItem::Unknown {
        hint_type: 0x02,
        value: Bytes::from_static(b"x"),
    };
    assert_eq!(
        r.process_message(Message::Bundle {
            // Deliberately out of type order, with the repeat last.
            hints: vec![stale, other.clone(), fresh.clone()],
            data: Bytes::from_static(b"hi"),
        }),
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from_static(b"hi"),
            hints: vec![other, fresh],
        }]
    );
}

#[test]
fn bare_frame_through_receive_pdu() {
    let mut r = default_receiver();
    let frame = bpv7_like(20);
    assert_eq!(
        r.receive_pdu(frame.clone()),
        vec![ReceiverEvent::BundleReceived {
            data: frame,
            hints: vec![],
        }]
    );
}

fn cancels(transfer_numbers: impl IntoIterator<Item = u32>) -> Bytes {
    let mut pdu = BytesMut::new();
    for transfer_number in transfer_numbers {
        pdu.put(encode(&Message::TransferCancel { transfer_number }));
    }
    pdu.freeze()
}

#[test]
fn every_message_in_a_pdu_can_produce_an_event() {
    let mut r = default_receiver();
    assert_eq!(
        r.receive_pdu(cancels(1000..1004)),
        (1000..1004)
            .map(|t| dropped(t, DropReason::UnknownTransfer))
            .collect::<Vec<_>>()
    );
}

#[test]
fn receive_pdu_into_replaces_the_callers_list() {
    let mut r = default_receiver();
    let mut events = vec![received(b"stale")];
    r.receive_pdu_into(cancels([7, 8]), &mut events);
    assert_eq!(
        events,
        vec![
            dropped(7, DropReason::UnknownTransfer),
            dropped(8, DropReason::UnknownTransfer),
        ]
    );

    r.receive_pdu_into(Bytes::new(), &mut events);
    assert_eq!(events, vec![]);
}

#[test]
fn receive_pdu_into_reuses_the_callers_allocation() {
    let mut r = default_receiver();
    let mut events = Vec::with_capacity(4);
    let allocation = events.as_ptr();
    r.receive_pdu_into(cancels(1000..1004), &mut events);
    assert_eq!(events.len(), 4);
    r.receive_pdu_into(cancels(2000..2003), &mut events);
    assert_eq!(
        events,
        (2000..2003)
            .map(|t| dropped(t, DropReason::UnknownTransfer))
            .collect::<Vec<_>>()
    );
    assert_eq!(events.as_ptr(), allocation);
}

#[test]
fn bundle_extent_hook_trims_padding_and_steps_over_mid_pdu_bundles() {
    // The caller's "peek": a 0x9F bundle is 20 bytes long here.
    let mut r = default_receiver()
        .with_bundle_extent(|b: &[u8]| (b.first() == Some(&0x9F) && b.len() >= 20).then_some(20));

    // A bare frame zero-filled to 46 bytes delivers exactly the bundle.
    let mut frame = BytesMut::from(bpv7_like(20).as_ref());
    frame.resize(46, 0);
    assert_eq!(
        r.receive_pdu(frame.freeze()),
        vec![ReceiverEvent::BundleReceived {
            data: bpv7_like(20),
            hints: vec![],
        }]
    );

    // An encapsulated bundle between two transfer messages is delivered
    // and the End behind it still completes the transfer (Section 7.3).
    let mut pdu = BytesMut::new();
    pdu.put_slice(&encode(&segment(0, 0, b"hel")));
    pdu.put_slice(&bpv7_like(20));
    pdu.put_slice(&encode(&end(0, 1, b"lo")));
    assert_eq!(
        r.receive_pdu(pdu.freeze()),
        vec![
            ReceiverEvent::BundleReceived {
                data: bpv7_like(20),
                hints: vec![],
            },
            received(b"hello"),
        ]
    );
}

#[test]
fn bundle_extent_hook_survives_its_own_panic() {
    // A hook that panics must not leave the receiver silently hookless: the
    // next bare frame must reach the same hook again rather than being
    // taken whole as a bundle.
    let mut r = default_receiver().with_bundle_extent(|_: &[u8]| -> Option<usize> {
        panic!("hook failed");
    });
    let frame = bpv7_like(8);
    for _ in 0..2 {
        let payload = catch_unwind(AssertUnwindSafe(|| r.receive_pdu(frame.clone())))
            .expect_err("the hook was not consulted");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"hook failed"));
    }
}

#[test]
fn mid_pdu_bundle_without_hook_ends_the_pdu() {
    let mut r = default_receiver();
    let mut pdu = BytesMut::new();
    pdu.put_slice(&encode(&segment(0, 0, b"hel")));
    let bundle_offset = pdu.len();
    pdu.put_slice(&bpv7_like(20));
    pdu.put_slice(&encode(&end(0, 1, b"lo")));
    let pdu = pdu.freeze();
    assert_eq!(
        r.receive_pdu(pdu.clone()),
        vec![ReceiverEvent::MalformedPdu {
            error: CodecError::EncapsulatedBundle {
                first_byte: 0x9F,
                offset: bundle_offset
            }
        }]
    );
    // The offset locates the discarded bytes in the caller's clone.
    assert!(pdu[bundle_offset..].starts_with(&bpv7_like(20)));
}

#[test]
fn sender_receiver_round_trip() {
    let mut s = sender(64, 16);
    let mut r = default_receiver();
    let original = Bytes::from(vec![0x42; 200]);
    s.enqueue(original.clone(), SendOptions::default()).unwrap();

    let mut all_events = Vec::new();
    while let Some(pdu) = s.next_pdu() {
        all_events.extend(r.receive_pdu(pdu.data));
    }
    assert_eq!(
        all_events,
        vec![ReceiverEvent::BundleReceived {
            data: original.clone(),
            hints: vec![HintItem::BundleLength(200)],
        }]
    );
}

#[test]
fn sender_receiver_round_trip_small() {
    let mut s = sender(256, 16);
    let mut r = default_receiver();
    s.enqueue(Bytes::from_static(b"tiny"), SendOptions::default())
        .unwrap();
    assert_eq!(
        r.receive_pdu(s.next_pdu().unwrap().data),
        vec![received(b"tiny")]
    );
}

// Drain `s` into `r` through the PDU entry point, collecting every event.
fn drain_into(s: &mut Sender, r: &mut Receiver) -> Vec<ReceiverEvent> {
    let mut events = Vec::new();
    while let Some(pdu) = s.next_pdu() {
        events.extend(r.receive_pdu(pdu.data));
    }
    events
}

fn max_bundle_size(v: usize) -> MaxBundleSize {
    MaxBundleSize::try_from(v).unwrap()
}

fn for_link(link_pdu_size: usize, cap: usize) -> u32 {
    MaxSegments::for_link_pdu_size(link_pdu_size, max_bundle_size(cap)).get()
}

#[test]
fn max_segments_zero_rejected() {
    assert_eq!(MaxSegments::new(0), None);
    assert_eq!(
        MaxSegments::try_from(0),
        Err(OutOfRange {
            name: "max segments per transfer",
            value: 0,
            min: 1,
            max: None,
        })
    );
    assert_eq!(MaxSegments::try_from(1), Ok(MaxSegments::MIN));
    assert_eq!(MaxSegments::try_from(u32::MAX), Ok(MaxSegments::MAX));
}

#[test]
fn max_segments_converts_to_and_from_non_zero() {
    let n = NonZeroU32::new(1500).unwrap();
    let limit = MaxSegments::from(n);
    assert_eq!(limit.get(), 1500);
    assert_eq!(NonZeroU32::from(limit), n);
    assert_eq!(u32::from(limit), 1500);
    assert_eq!(limit.to_string(), "1500");
}

#[test]
fn max_segments_parses_and_formats_as_its_integer() {
    assert_eq!("1500".parse(), Ok(MaxSegments::try_from(1500).unwrap()));
    assert_eq!(
        "0".parse::<MaxSegments>(),
        Err(ParseError::OutOfRange(
            MaxSegments::try_from(0).unwrap_err()
        ))
    );
    assert_eq!(
        "".parse::<MaxSegments>(),
        Err(ParseError::Syntax {
            name: "max segments per transfer",
            source: "".parse::<u32>().unwrap_err(),
        })
    );
    let m = MaxSegments::MAX;
    assert_eq!(format!("{m:x} {m:X}"), "ffffffff FFFFFFFF");
}

#[test]
fn link_derived_limit_is_four_times_the_reference_count() {
    // 1036-byte PDUs carry 1024 data bytes per segment: 1 MiB is 1024
    // segments, so the limit is 4096.
    assert_eq!(for_link(1036, 1 << 20), 4096);
    // One byte more needs a 1025th segment.
    assert_eq!(for_link(1036, (1 << 20) + 1), 4100);
}

#[test]
fn link_derived_limit_never_falls_below_64() {
    assert_eq!(for_link(1036, 100), 64);
    assert_eq!(for_link(1036, 1), 64);
}

#[test]
fn link_derived_limit_of_a_pdu_no_larger_than_the_framing_assumes_one_byte_segments() {
    // A PDU of 12 bytes or fewer has no room for segment data; the
    // reference capacity is one byte rather than a division by zero.
    for pdu in [0, 1, 11, 12, 13] {
        assert_eq!(for_link(pdu, 1000), 4000, "PDU size {pdu}");
    }
}

#[test]
fn link_derived_limit_caps_segment_data_at_the_content_length_ceiling() {
    // A PDU beyond one message's ceiling still carries at most 1048567
    // data bytes per segment (20-bit content length less 8 bytes of
    // transfer number and index).
    assert_eq!(for_link(usize::MAX, 1_048_567 * 100), 400);
}

#[test]
fn link_derived_limit_saturates_at_u32_max() {
    assert_eq!(for_link(1, usize::MAX), u32::MAX);
    // 2^30 one-byte segments, times four, is 2^32: one past u32::MAX.
    assert_eq!(for_link(13, 1 << 30), u32::MAX);
}

#[test]
fn small_pdu_link_delivers_a_bundle_the_per_segment_charge_rejects() {
    let link_48 = MaxSegments::for_link_pdu_size(48, max_bundle_size(10_000));
    // 48-byte PDUs carry 36 data bytes per segment (26 in the first, which
    // carries the Bundle Length hint), so a 9000-byte bundle is 251
    // segments.  Charged 64 bytes each, they exceed a 10000-byte budget;
    // limited by count instead, they are well within 4 × 278.
    let bundle = Bytes::from(vec![0x42; 9000]);
    let expected = vec![ReceiverEvent::BundleReceived {
        data: bundle.clone(),
        hints: vec![HintItem::BundleLength(9000)],
    }];

    let mut s = sender(48, 16);
    s.enqueue(bundle.clone(), SendOptions::default()).unwrap();
    let mut r = receiver_with_max_segments(16, 10_000, link_48);
    assert_eq!(drain_into(&mut s, &mut r), expected);

    let mut s = sender(48, 16);
    s.enqueue(bundle, SendOptions::default()).unwrap();
    let mut r = receiver(16, 10_000);
    let events = drain_into(&mut s, &mut r);
    assert_eq!(events[0], rejected(0, RejectReason::TooFragmented));
    assert!(
        events[1..]
            .iter()
            .all(|e| *e == dropped(0, DropReason::Rejected(RejectReason::TooFragmented))),
        "{events:?}"
    );
}

#[test]
fn bundle_of_exactly_the_cap_is_delivered_with_a_link_derived_limit() {
    let link_48 = MaxSegments::for_link_pdu_size(48, max_bundle_size(10_000));
    let bundle = Bytes::from(vec![0x42; 10_000]);
    let mut s = sender(48, 16);
    s.enqueue(bundle.clone(), SendOptions::default()).unwrap();
    let mut r = receiver_with_max_segments(16, 10_000, link_48);
    assert_eq!(
        drain_into(&mut s, &mut r),
        vec![ReceiverEvent::BundleReceived {
            data: bundle,
            hints: vec![HintItem::BundleLength(10_000)],
        }]
    );
}

// Empty segments count toward the limit without counting toward the
// 100-byte cap.
const LIMIT: u32 = 400;

fn receiver_limited_to(limit: u32) -> Receiver {
    receiver_with_max_segments(16, 100, MaxSegments::try_from(limit).unwrap())
}

fn at_the_limit() -> Receiver {
    let mut r = receiver_limited_to(LIMIT);
    for i in 0..LIMIT {
        assert_eq!(r.process_message(segment(0, i, b"")), vec![], "segment {i}");
    }
    r
}

#[test]
fn transfer_of_exactly_the_segment_limit_completes() {
    let mut r = receiver_limited_to(LIMIT);
    // The End first: its segment counts, and completion still works when
    // the earlier segments follow.
    assert_eq!(r.process_message(end(0, LIMIT - 1, b"x")), vec![]);
    for i in 0..LIMIT - 2 {
        assert_eq!(r.process_message(segment(0, i, b"")), vec![]);
    }
    assert_eq!(
        r.process_message(segment(0, LIMIT - 2, b"")),
        vec![received(b"x")]
    );
}

#[test]
fn segment_beyond_the_limit_rejects_the_transfer_as_too_fragmented() {
    let mut r = at_the_limit();
    assert_eq!(
        r.process_message(segment(0, LIMIT, b"")),
        vec![rejected(0, RejectReason::TooFragmented)]
    );
    // Later messages do not re-create it.
    assert_eq!(
        r.process_message(segment(0, 0, b"")),
        vec![dropped(
            0,
            DropReason::Rejected(RejectReason::TooFragmented)
        )]
    );
    assert_eq!(
        r.process_message(end(0, LIMIT + 1, b"")),
        vec![dropped(
            0,
            DropReason::Rejected(RejectReason::TooFragmented)
        )]
    );
}

#[test]
fn repeated_segments_do_not_consume_the_allowance() {
    let mut r = at_the_limit();
    for i in [0, LIMIT / 2, LIMIT - 1] {
        assert_eq!(r.process_message(segment(0, i, b"")), vec![]);
    }
    // Repeating the final index as an End is not a new segment either: it
    // completes the transfer rather than rejecting it as fragmented.  With
    // every segment empty there is no bundle to deliver.
    assert_eq!(
        r.process_message(end(0, LIMIT - 1, b"")),
        vec![rejected(0, RejectReason::EmptyBundle)]
    );
}

#[test]
fn transfer_end_cannot_bypass_the_segment_limit() {
    let mut r = at_the_limit();
    assert_eq!(
        r.process_message(end(0, LIMIT, b"")),
        vec![rejected(0, RejectReason::TooFragmented)]
    );
}

#[test]
fn too_large_takes_precedence_over_too_fragmented() {
    // The segment over the limit also carries the cap past its bytes.
    let mut r = at_the_limit();
    assert_eq!(
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: LIMIT,
            hints: vec![],
            data: Bytes::from(vec![0; 101]),
        })),
        vec![rejected(0, RejectReason::TooLarge)]
    );
}

#[test]
fn bundle_length_hint_does_not_raise_the_segment_limit() {
    let mut r = receiver_limited_to(LIMIT);
    r.process_message(Message::TransferSegment(TransferSegmentMessage {
        transfer_number: 0,
        segment_index: 0,
        hints: vec![HintItem::BundleLength(100)],
        data: Bytes::new(),
    }));
    for i in 1..LIMIT {
        assert_eq!(r.process_message(segment(0, i, b"")), vec![]);
    }
    assert_eq!(
        r.process_message(segment(0, LIMIT, b"")),
        vec![rejected(0, RejectReason::TooFragmented)]
    );
}

#[test]
fn large_pdus_do_not_raise_the_segment_limit() {
    // The same limit through receive_pdu, with every segment in a 4 KiB
    // PDU.
    let mut r = receiver_limited_to(LIMIT);
    let padded = |msg: Message| {
        let mut pdu = BytesMut::from(&encode(&msg)[..]);
        pdu.resize(4096, 0);
        pdu.freeze()
    };
    for i in 0..LIMIT {
        assert_eq!(r.receive_pdu(padded(segment(0, i, b""))), vec![]);
    }
    assert_eq!(
        r.receive_pdu(padded(segment(0, LIMIT, b""))),
        vec![rejected(0, RejectReason::TooFragmented)]
    );
}

#[test]
fn without_a_segment_limit_the_per_segment_charge_still_applies() {
    // The same empty segments, charged 64 bytes each against the 4 KiB
    // floor, are refused at the 65th.
    let mut r = receiver(16, 100);
    let floor = (MIN_OVERHEAD_BUDGET / SEGMENT_OVERHEAD) as u32;
    for i in 0..floor {
        assert_eq!(r.process_message(segment(0, i, b"")), vec![]);
    }
    assert_eq!(
        r.process_message(segment(0, floor, b"")),
        vec![rejected(0, RejectReason::TooFragmented)]
    );
}

#[test]
fn debug_names_closed_transfers_by_wire_number() {
    let mut r = receiver(4, 1024);
    r.process_message(end(7, 0, b"done"));
    r.process_message(segment(8, 0, b"open"));
    assert_eq!(
        format!("{r:?}"),
        "Receiver { max_bundle_size: MaxBundleSize(1024), segment_limit: None, \
         max_retained_bytes: MaxRetainedBytes(5120), retained: 68, fec: false, \
         bundle_extent: false, window: TransferWindow { greatest: Some(WindowKey(8)), \
         window_size: WindowSize(4) }, transfers: 1, closed: {WindowKey(7): Delivered} }"
    );
}

#[test]
fn max_retained_bytes_zero_rejected() {
    assert_eq!(MaxRetainedBytes::new(0), None);
    assert_eq!(
        MaxRetainedBytes::try_from(0),
        Err(OutOfRange {
            name: "max retained bytes",
            value: 0,
            min: 1,
            max: None,
        })
    );
    assert_eq!(MaxRetainedBytes::try_from(1), Ok(MaxRetainedBytes::MIN));
    assert_eq!(
        MaxRetainedBytes::try_from(usize::MAX),
        Ok(MaxRetainedBytes::MAX)
    );
}

#[test]
fn max_retained_bytes_converts_parses_and_formats_as_its_integer() {
    let n = NonZeroUsize::new(65_536).unwrap();
    let limit = MaxRetainedBytes::from(n);
    assert_eq!(limit.get(), 65_536);
    assert_eq!(NonZeroUsize::from(limit), n);
    assert_eq!(usize::from(limit), 65_536);
    assert_eq!(limit.to_string(), "65536");
    assert_eq!(format!("{limit:x}"), "10000");
    assert_eq!("65536".parse(), Ok(limit));
    assert_eq!(
        "0".parse::<MaxRetainedBytes>(),
        Err(ParseError::OutOfRange(
            MaxRetainedBytes::try_from(0).unwrap_err()
        ))
    );
    assert_eq!(
        "".parse::<MaxRetainedBytes>(),
        Err(ParseError::Syntax {
            name: "max retained bytes",
            source: "".parse::<usize>().unwrap_err(),
        })
    );
}

#[test]
fn retention_floor_is_one_transfers_full_allowance() {
    // The cap of segment data plus a bookkeeping budget of the cap...
    assert_eq!(
        MaxRetainedBytes::min_for(MaxBundleSize::DEFAULT, None).get(),
        2 * MaxBundleSize::DEFAULT.get()
    );
    // ...or of MIN_OVERHEAD_BUDGET under a small cap.
    assert_eq!(
        MaxRetainedBytes::min_for(max_bundle_size(100), None).get(),
        100 + MIN_OVERHEAD_BUDGET
    );
    assert_eq!(
        MaxRetainedBytes::min_for(MaxBundleSize::MAX, None),
        MaxRetainedBytes::MAX
    );
    // With a segment limit: every allowed segment's overhead, plus one
    // maximal hint of each of the 128 types...
    let limit = MaxSegments::try_from(1000).unwrap();
    assert_eq!(
        MaxRetainedBytes::min_for(MaxBundleSize::DEFAULT, Some(limit)).get(),
        MaxBundleSize::DEFAULT.get() + 1000 * SEGMENT_OVERHEAD + 128 * (2 + 255)
    );
    // ...or the smaller bookkeeping budget, which hints cannot exceed.
    assert_eq!(
        MaxRetainedBytes::min_for(max_bundle_size(100), Some(limit)).get(),
        100 + 1000 * SEGMENT_OVERHEAD + MIN_OVERHEAD_BUDGET
    );
}

fn receiver_with_retention(
    window: u16,
    cap: usize,
    max_retained_bytes: Option<MaxRetainedBytes>,
) -> Receiver {
    Receiver::new(ReceiverConfig {
        window_size: window_size(window),
        max_bundle_size: max_bundle_size(cap),
        max_retained_bytes,
        ..ReceiverConfig::default()
    })
}

// A segment of `len` zero bytes, charged `len + SEGMENT_OVERHEAD`.
fn zeros(transfer_number: u32, segment_index: u32, len: usize) -> Message {
    Message::TransferSegment(TransferSegmentMessage {
        transfer_number,
        segment_index,
        hints: vec![],
        data: Bytes::from(vec![0; len]),
    })
}

// An End carrying `len` zero bytes.
fn zeros_end(transfer_number: u32, segment_index: u32, len: usize) -> Message {
    Message::TransferEnd(TransferSegmentMessage {
        transfer_number,
        segment_index,
        hints: vec![],
        data: Bytes::from(vec![0; len]),
    })
}

// The retention limit a 4 KiB cap gets by default: 4 KiB of data plus the
// 4 KiB MIN_OVERHEAD_BUDGET.
const CAP: usize = 4096;
const FLOOR: usize = 2 * CAP;
const _: () = assert!(MIN_OVERHEAD_BUDGET == CAP);

#[test]
fn transfer_that_would_exceed_the_retention_limit_is_rejected() {
    let mut r = receiver_with_retention(16, CAP, None);
    let held = 3900 + SEGMENT_OVERHEAD;
    assert_eq!(r.process_message(zeros(0, 0, 3900)), vec![]);
    assert_eq!(r.process_message(zeros(1, 0, 3900)), vec![]);

    // A third transfer, within its own limits, would take the total over.
    assert!(2 * held + 300 + SEGMENT_OVERHEAD > FLOOR);
    assert_eq!(
        r.process_message(zeros(2, 0, 300)),
        vec![rejected(2, RejectReason::ReceiverFull)]
    );
    assert_eq!(
        r.process_message(zeros(2, 1, 1)),
        vec![dropped(2, DropReason::Rejected(RejectReason::ReceiverFull))]
    );

    // The transfers already held are kept and complete.
    let mut bundle = vec![0; 3900];
    bundle.push(b'x');
    assert_eq!(
        r.process_message(end(0, 1, b"x")),
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from(bundle),
            hints: vec![],
        }]
    );

    // Delivery released transfer 0's charge.
    assert_eq!(r.process_message(zeros(3, 0, 3900)), vec![]);
}

#[test]
fn retention_is_released_by_cancel_expiry_and_rejection() {
    let mut r = receiver_with_retention(4, CAP, None);
    r.process_message(zeros(0, 0, 3900));
    r.process_message(zeros(1, 0, 3900));

    assert_eq!(
        r.process_message(Message::TransferCancel { transfer_number: 1 }),
        vec![ReceiverEvent::TransferCancelled { transfer_number: 1 }]
    );
    assert_eq!(r.process_message(zeros(2, 0, 3900)), vec![]);

    // Transfer 4 expires transfer 0, and fits in the space it leaves.
    assert_eq!(
        r.process_message(zeros(4, 0, 3900)),
        vec![ReceiverEvent::TransferExpired { transfer_number: 0 }]
    );

    // Rejecting transfer 4 as over its cap leaves room for transfer 3.
    assert_eq!(
        r.process_message(zeros(4, 1, 300)),
        vec![rejected(4, RejectReason::TooLarge)]
    );
    assert_eq!(r.process_message(zeros(3, 0, 3900)), vec![]);
}

#[test]
fn retention_limit_below_the_floor_is_raised_to_it() {
    // A cap-sized bundle is charged more than the cap, for its segments'
    // bookkeeping, and still fits.
    let mut r = receiver_with_retention(16, CAP, Some(MaxRetainedBytes::MIN));
    r.process_message(zeros(0, 0, CAP - 96));
    let events = r.process_message(zeros_end(0, 1, 96));
    assert_eq!(
        events,
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from(vec![0; CAP]),
            hints: vec![],
        }]
    );

    // And the floor, not the configured value, is the limit.
    r.process_message(zeros(1, 0, 3900));
    r.process_message(zeros(2, 0, 3900));
    assert_eq!(
        r.process_message(zeros(3, 0, 300)),
        vec![rejected(3, RejectReason::ReceiverFull)]
    );
}

#[test]
fn retention_limit_above_the_floor_is_honoured() {
    let limit = MaxRetainedBytes::try_from(3 * (3900 + SEGMENT_OVERHEAD)).unwrap();
    let mut r = receiver_with_retention(16, CAP, Some(limit));
    for transfer_number in 0..3 {
        assert_eq!(r.process_message(zeros(transfer_number, 0, 3900)), vec![]);
    }
    assert_eq!(
        r.process_message(zeros(3, 0, 1)),
        vec![rejected(3, RejectReason::ReceiverFull)]
    );
}

#[test]
fn empty_segments_count_against_the_retention_limit_under_a_segment_limit() {
    // Floor: 100 bytes of data, 10 segments' overhead, and a 4 KiB hint
    // allowance.  Each transfer of ten empty segments is charged 640 bytes.
    let limit = MaxSegments::try_from(10).unwrap();
    let floor = MaxRetainedBytes::min_for(max_bundle_size(100), Some(limit)).get();
    assert_eq!(floor, 100 + 10 * SEGMENT_OVERHEAD + MIN_OVERHEAD_BUDGET);
    let mut r = receiver_with_max_segments(16, 100, limit);
    let mut charged = 0;
    for transfer_number in 0.. {
        for segment_index in 0..10 {
            let events = r.process_message(segment(transfer_number, segment_index, b""));
            charged += SEGMENT_OVERHEAD;
            if charged > floor {
                assert_eq!(
                    (transfer_number, segment_index, events),
                    (7, 5, vec![rejected(7, RejectReason::ReceiverFull)])
                );
                return;
            }
            assert_eq!(events, vec![]);
        }
    }
}
