//! Segmentation, PDU packing, window release, and link framing through the
//! public `sender` API.

mod common;

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    num::NonZeroUsize,
    slice::from_ref,
};

use bytes::Bytes;
use hardy_btpu::{
    OutOfRange, ParseError,
    codec::{
        decode_pdu,
        header::{HEADER_SIZE, MAX_CONTENT_LENGTH},
        hint::{HintItem, HintType, HintValue, encoded_hints_len},
        message::{FrameKind, Message, TransferSegmentMessage, frame_kind},
    },
    sender::{
        BundleFraming, Carried, CarriedList, Error, LinkFraming, PduSize, SendId, SendOptions,
        SendQueueDepth, Sender, SenderConfig,
    },
    transfer::Error as TransferError,
};

use self::common::{
    bpv7_like, bundle, bundle_with, cancel, carried, drain, enqueue, sender, sender_config,
    sender_with_queue_depth, unknown_hint, window_size,
};

// The transfer number and segment index every segment message carries.
const SEGMENT_FIELDS: usize = 8;

// Unwrap the segmented case; panics on an unsegmented bundle.
fn transfer_number(id: SendId) -> u32 {
    match id {
        SendId::Transfer(t) => t,
        SendId::Message(_) | SendId::Bare(_) => {
            panic!("expected a segmented transfer")
        }
    }
}

fn decode_all(pdu: Bytes) -> Vec<Message> {
    decode_pdu(pdu).collect::<Result<_, _>>().unwrap()
}

fn variable_sender(pdu_size: usize, bundle_framing: BundleFraming) -> Sender {
    Sender::new(
        SenderConfig {
            link_framing: LinkFraming::Variable { bundle_framing },
            ..sender_config(pdu_size, 16)
        },
        0,
    )
}

#[test]
fn config_defaults() {
    let c = SenderConfig::default();
    assert_eq!(c.pdu_size.get(), 1500);
    assert_eq!(c.window_size.get(), 16);
    assert_eq!(c.send_queue_depth.get(), 64);
    assert_eq!(c.link_framing, LinkFraming::FixedSize);
}

#[test]
fn pdu_size_boundaries() {
    assert_eq!(PduSize::MIN.get(), HEADER_SIZE);
    assert_eq!(PduSize::MAX.get(), HEADER_SIZE + MAX_CONTENT_LENGTH);
    assert_eq!(PduSize::try_from(HEADER_SIZE), Ok(PduSize::MIN));
    assert_eq!(
        PduSize::try_from(HEADER_SIZE + MAX_CONTENT_LENGTH),
        Ok(PduSize::MAX)
    );
    let out_of_range = |value: usize| OutOfRange {
        name: "PDU size",
        value: value as u64,
        min: HEADER_SIZE as u64,
        max: Some((HEADER_SIZE + MAX_CONTENT_LENGTH) as u64),
    };
    for value in [0, HEADER_SIZE - 1, HEADER_SIZE + MAX_CONTENT_LENGTH + 1] {
        assert_eq!(PduSize::new(value), None);
        assert_eq!(PduSize::try_from(value), Err(out_of_range(value)));
    }
}

#[test]
fn pdu_size_default_is_the_const() {
    assert_eq!(PduSize::default(), PduSize::DEFAULT);
    assert_eq!(PduSize::DEFAULT.get(), 1500);
    assert_eq!(PduSize::new(1500), Some(PduSize::DEFAULT));
    assert_eq!(PduSize::DEFAULT.to_string(), "1500");
}

#[test]
fn pdu_size_parses_and_formats_as_its_integer() {
    assert_eq!("1500".parse(), Ok(PduSize::DEFAULT));
    assert_eq!(
        "3".parse::<PduSize>(),
        Err(ParseError::OutOfRange(PduSize::try_from(3).unwrap_err()))
    );
    assert_eq!(
        "3".parse::<PduSize>().unwrap_err().to_string(),
        "Invalid PDU size 3 (must be 4..=1048579)"
    );
    let source = "1.5k".parse::<usize>().unwrap_err();
    assert_eq!(
        "1.5k".parse::<PduSize>(),
        Err(ParseError::Syntax {
            name: "PDU size",
            source: source.clone(),
        })
    );
    assert_eq!(
        "1.5k".parse::<PduSize>().unwrap_err().to_string(),
        format!("Invalid PDU size: {source}")
    );
    let p = PduSize::DEFAULT;
    assert_eq!(
        format!("{p} {p:b} {p:o} {p:x} {p:#X} {p:>6}"),
        "1500 10111011100 2734 5dc 0x5DC   1500"
    );
}

#[test]
fn send_queue_depth_zero_rejected() {
    assert_eq!(SendQueueDepth::new(0), None);
    assert_eq!(
        SendQueueDepth::try_from(0),
        Err(OutOfRange {
            name: "send queue depth",
            value: 0,
            min: 1,
            max: None,
        })
    );
    assert_eq!(
        SendQueueDepth::try_from(0).unwrap_err().to_string(),
        "Invalid send queue depth 0 (must be at least 1)"
    );
    assert_eq!(SendQueueDepth::try_from(1), Ok(SendQueueDepth::MIN));
    assert_eq!(
        SendQueueDepth::try_from(usize::MAX),
        Ok(SendQueueDepth::MAX)
    );
}

#[test]
fn send_queue_depth_converts_to_and_from_non_zero() {
    let n = NonZeroUsize::new(7).unwrap();
    let depth = SendQueueDepth::from(n);
    assert_eq!(depth.get(), 7);
    assert_eq!(NonZeroUsize::from(depth), n);
    assert_eq!(usize::from(depth), 7);
    assert_eq!(depth.to_string(), "7");
    assert_eq!(SendQueueDepth::default(), SendQueueDepth::DEFAULT);
    assert_eq!(SendQueueDepth::DEFAULT.get(), 64);
}

#[test]
fn send_queue_depth_parses_and_formats_as_its_integer() {
    assert_eq!("64".parse(), Ok(SendQueueDepth::DEFAULT));
    assert_eq!(
        "0".parse::<SendQueueDepth>(),
        Err(ParseError::OutOfRange(
            SendQueueDepth::try_from(0).unwrap_err()
        ))
    );
    assert_eq!(
        "-1".parse::<SendQueueDepth>(),
        Err(ParseError::Syntax {
            name: "send queue depth",
            source: "-1".parse::<usize>().unwrap_err(),
        })
    );
    let d = SendQueueDepth::DEFAULT;
    assert_eq!(format!("{d:b} {d:o} {d:x} {d:X}"), "1000000 100 40 40");
}

#[test]
fn empty_bundle_rejected_at_enqueue() {
    let mut s = sender(256, 16);
    assert_eq!(enqueue(&mut s, Bytes::new()), Err(Error::Empty));
    assert!(!s.has_pending());
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn minimum_pdu_size_cannot_queue_an_undrainable_message() {
    // At PduSize::MIN only a zero-content Bundle message would fit, and
    // empty bundles are rejected; a one-byte bundle must take the
    // segmentation path and fail cleanly rather than queue a message
    // larger than any PDU (which would make the drain loop spin).
    let mut s = sender(PduSize::MIN.get(), 16);
    assert_eq!(
        enqueue(&mut s, Bytes::from_static(b"x")),
        Err(Error::PduTooSmall {
            required: HEADER_SIZE
                + SEGMENT_FIELDS
                + encoded_hints_len(&[HintItem::BundleLength(1)])
                + 1,
            pdu_size: PduSize::MIN.get(),
        })
    );
    assert!(!s.has_pending());
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn segmenting_floor_grows_with_the_bundle_length_hint() {
    // The derived Bundle Length hint on the first segment takes 1, 2, or 4
    // value bytes as the bundle grows (8 past 4 GiB, too large to queue
    // here), and one data byte must fit beside it.
    for (len, floor) in [(255, 16), (256, 17), (65_535, 17), (65_536, 19)] {
        assert_eq!(
            enqueue(&mut sender(floor - 1, 4), bundle(len)),
            Err(Error::PduTooSmall {
                required: floor,
                pdu_size: floor - 1,
            }),
            "{len}-byte bundle"
        );
        assert_eq!(
            enqueue(&mut sender(floor, 4), bundle(len)),
            Ok(SendId::Transfer(0))
        );
    }
}

#[test]
fn pdu_too_small_to_segment_leaves_window_untouched() {
    // A hint chain that leaves no room for segment data fails before a
    // transfer number is taken, so the next bundle still gets number 0.
    let mut s = sender(64, 4);
    let wide = HintItem::Unknown {
        hint_type: HintType::new(0x41).unwrap(),
        value: HintValue::new(Bytes::from(vec![0u8; 60])).unwrap(),
    };
    assert!(matches!(
        s.enqueue(
            bundle(200),
            SendOptions {
                hints: vec![wide].into()
            }
        ),
        Err(Error::PduTooSmall { pdu_size: 64, .. })
    ));
    assert!(s.is_window_available());
    assert!(!s.has_pending());
    assert_eq!(enqueue(&mut s, bundle(200)), Ok(SendId::Transfer(0)));
}

#[test]
fn caller_hints_ride_first_segment_with_derived_bundle_length() {
    let mut s = sender(64, 4);
    let correlator = unknown_hint(0x41, b"\x01\x02");
    // The caller-supplied BundleLength is discarded; the sender derives
    // the truthful one and puts it first.
    let options = SendOptions {
        hints: vec![HintItem::BundleLength(999), correlator.clone()].into(),
    };
    s.enqueue(bundle(200), options).unwrap();

    let first = decode_pdu(s.next_pdu().unwrap().data)
        .next()
        .unwrap()
        .unwrap();
    let Message::TransferSegment(m) = first else {
        panic!("expected a leading Transfer Segment, got {first:?}")
    };
    assert_eq!(m.hints, vec![HintItem::BundleLength(200), correlator]);
    // The first segment's data capacity shrinks by exactly the merged
    // hint chain's encoded size.
    assert_eq!(
        m.data.len(),
        64 - HEADER_SIZE - SEGMENT_FIELDS - encoded_hints_len(&m.hints)
    );
}

#[test]
fn repeated_caller_hint_types_go_out_once_latest_wins() {
    let mut s = sender(64, 4);
    let options = SendOptions {
        hints: vec![unknown_hint(0x41, b"old"), unknown_hint(0x41, b"new")].into(),
    };
    s.enqueue(bundle(200), options).unwrap();

    let first = decode_pdu(s.next_pdu().unwrap().data)
        .next()
        .unwrap()
        .unwrap();
    let Message::TransferSegment(m) = first else {
        panic!("expected a leading Transfer Segment, got {first:?}")
    };
    // The decoder folds repeats too, so the data length is what shows the
    // repeat never reached the wire: segment 0 spends only one item's bytes.
    let sent = [HintItem::BundleLength(200), unknown_hint(0x41, b"new")];
    assert_eq!(m.hints, sent);
    assert_eq!(
        m.data.len(),
        64 - HEADER_SIZE - SEGMENT_FIELDS - encoded_hints_len(&sent)
    );
}

#[test]
fn first_segment_capacity_reduced_by_exactly_the_hint_bytes() {
    let pdu_size = 32;
    let mut s = sender(pdu_size, 16);
    enqueue(&mut s, Bytes::from(vec![0xAB; 100])).unwrap();

    let messages: Vec<Message> = drain(&mut s).into_iter().flat_map(decode_all).collect();

    // Every segment carries a fixed overhead of the 4-byte message header
    // plus the 8-byte transfer number + segment index prefix.
    let full_capacity = pdu_size - HEADER_SIZE - SEGMENT_FIELDS;

    let mut segments: Vec<&TransferSegmentMessage> = messages
        .iter()
        .filter_map(|m| match m {
            Message::TransferSegment(seg) => Some(seg),
            _ => None,
        })
        .collect();
    segments.sort_by_key(|seg| seg.segment_index);
    let (first, middle) = segments.split_first().unwrap();
    assert_eq!(first.segment_index, 0);

    // The first segment cedes exactly the encoded hint bytes to the
    // Bundle Length hint...
    let hint_len = encoded_hints_len(&first.hints);
    assert!(hint_len > 0);
    assert_eq!(first.data.len(), full_capacity - hint_len);

    // ...while every hintless middle segment fills its PDU exactly.
    assert!(!middle.is_empty());
    for seg in middle {
        assert!(seg.hints.is_empty());
        assert_eq!(
            seg.data.len(),
            full_capacity,
            "segment {}",
            seg.segment_index
        );
    }
}

#[test]
fn caller_hints_ride_unsegmented_bundle_message() {
    let mut s = sender(64, 4);
    let hint = unknown_hint(0x41, b"z");
    let data = Bytes::from_static(b"tiny");
    assert_eq!(
        s.enqueue(
            data.clone(),
            SendOptions {
                hints: vec![hint.clone()].into(),
            },
        ),
        Ok(SendId::Message(0))
    );

    let msg = decode_pdu(s.next_pdu().unwrap().data)
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(msg, bundle_with(vec![hint], data));
}

#[test]
fn max_pdu_size_bundle_encodes_without_panic() {
    // Regression: with pdu_size at the limit, both the largest possible
    // Bundle message and the segmentation path must stay within the
    // 20-bit content length; next_pdu must never hit its expect().
    let mut s = sender(PduSize::MAX.get(), 16);

    // Largest bundle that fits unsegmented: content == MAX_CONTENT_LENGTH.
    assert_eq!(
        enqueue(&mut s, bundle(MAX_CONTENT_LENGTH)),
        Ok(SendId::Message(0))
    );
    // One byte more: must segment, and every segment must encode.
    assert_eq!(
        enqueue(&mut s, bundle(MAX_CONTENT_LENGTH + 1)),
        Ok(SendId::Transfer(0))
    );

    let pdus = drain(&mut s);
    // The unsegmented bundle fills one PDU, and the other two segments.
    assert_eq!(pdus.len(), 3);
    for pdu in &pdus {
        assert_eq!(pdu.len(), PduSize::MAX.get());
    }
}

#[test]
fn small_bundle_no_segmentation() {
    let mut s = sender(256, 16);
    let data = Bytes::from_static(b"hello");
    let len = data.len();
    assert_eq!(enqueue(&mut s, data.clone()), Ok(SendId::Message(0)));

    let pdu = s.next_pdu().unwrap().data;
    assert_eq!(pdu.len(), 256);
    assert_eq!(
        decode_all(pdu),
        vec![
            bundle_with(vec![], data),
            Message::DefinitePadding {
                len: 256 - HEADER_SIZE - len - HEADER_SIZE
            },
        ]
    );
}

#[test]
fn large_bundle_segmented() {
    let pdu_size = 32;
    let mut s = sender(pdu_size, 16);
    let data = Bytes::from(vec![0xAB; 100]);
    assert_eq!(enqueue(&mut s, data.clone()), Ok(SendId::Transfer(0)));

    let mut all_messages = Vec::new();
    for pdu in drain(&mut s) {
        assert_eq!(pdu.len(), pdu_size);
        all_messages.extend(decode_all(pdu));
    }

    // Segments in index order, one End, and the data reassembles.
    let mut indexed: Vec<(u32, Bytes)> = Vec::new();
    let mut ends = 0;
    for msg in &all_messages {
        match msg {
            Message::TransferSegment(m) => indexed.push((m.segment_index, m.data.clone())),
            Message::TransferEnd(m) => {
                ends += 1;
                indexed.push((m.segment_index, m.data.clone()));
            }
            Message::DefinitePadding { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(ends, 1);
    let indices: Vec<u32> = indexed.iter().map(|(i, _)| *i).collect();
    assert_eq!(indices, (0..indices.len() as u32).collect::<Vec<_>>());
    let combined: Vec<u8> = indexed.into_iter().flat_map(|(_, d)| d.to_vec()).collect();
    assert_eq!(combined, data.to_vec());
}

#[cfg(feature = "rand")]
#[test]
fn from_rng_seeds_initial_transfer_number() {
    let mut s = Sender::from_rng(sender_config(64, 16), &mut common::FixedRng(0xDEAD_BEEF));
    assert_eq!(
        enqueue(&mut s, bundle(200)),
        Ok(SendId::Transfer(0xDEAD_BEEF))
    );
}

#[cfg(feature = "rand")]
#[test]
fn try_from_rng_seeds_initial_transfer_number() {
    let mut s =
        Sender::try_from_rng(sender_config(64, 16), &mut common::FixedRng(0xDEAD_BEEF)).unwrap();
    assert_eq!(
        enqueue(&mut s, bundle(200)),
        Ok(SendId::Transfer(0xDEAD_BEEF))
    );
}

#[cfg(feature = "rand")]
#[test]
fn try_from_rng_returns_the_rng_error() {
    assert_eq!(
        Sender::try_from_rng(sender_config(64, 16), &mut common::FailingRng).err(),
        Some(common::RngFailure)
    );
}

#[test]
fn window_slot_is_released_when_the_transfer_end_is_packed() {
    let mut s = sender(64, 4);
    for expected in 0..4u32 {
        assert_eq!(enqueue(&mut s, bundle(200)), Ok(SendId::Transfer(expected)));
    }
    assert!(!s.is_window_available());
    assert_eq!(
        enqueue(&mut s, bundle(200)),
        Err(Error::Window(TransferError::WindowFull {
            window_size: window_size(4)
        }))
    );
    assert!(s.is_transfer_outstanding(0));

    // Drain PDU by PDU: the window opens exactly when transfer 0's End
    // leaves the queue, with no call from the caller.
    let mut opened_on_end = false;
    while !s.is_window_available() {
        let pdu = s
            .next_pdu()
            .expect("window cannot open with nothing pending")
            .data;
        let packed_end_of_0 = decode_all(pdu)
            .iter()
            .any(|m| matches!(m, Message::TransferEnd(e) if e.transfer_number == 0));
        if s.is_window_available() {
            opened_on_end = packed_end_of_0;
        } else {
            assert!(
                !packed_end_of_0,
                "End of transfer 0 packed but window still closed"
            );
        }
    }
    assert!(
        opened_on_end,
        "window opened before transfer 0's End was packed"
    );
    assert!(!s.is_transfer_outstanding(0));
    assert!(s.is_transfer_outstanding(1));
    assert_eq!(enqueue(&mut s, bundle(200)), Ok(SendId::Transfer(4)));
}

#[test]
fn cancelling_the_newest_transfer_keeps_the_window_span() {
    // Section 5: the sender MUST NOT emit a message whose transfer number
    // is <= greatest - window_size.  Freeing the newest transfer while the
    // oldest is outstanding must not admit a new number, or a later
    // message for the oldest would fall outside the receiver's window.
    let mut s = sender(64, 4);
    for _ in 0..4 {
        transfer_number(enqueue(&mut s, bundle(200)).unwrap());
    }
    assert!(s.cancel(SendId::Transfer(3)));
    assert!(!s.is_window_available());
    assert!(matches!(
        enqueue(&mut s, bundle(200)),
        Err(Error::Window(TransferError::WindowFull { .. }))
    ));

    // Releasing the oldest advances the window base; 4 is now admissible
    // and every still-active number (1, 2) stays within 4 - 4 + 1..=4.
    assert!(s.cancel(SendId::Transfer(0)));
    assert!(s.is_window_available());
    assert_eq!(enqueue(&mut s, bundle(200)), Ok(SendId::Transfer(4)));
}

#[test]
fn cancel_before_any_emission_queues_no_cancel_message() {
    // Nothing of the transfer reached the link, so the receiver never
    // learned of it and a Cancel would only be ignored (Section 8.4).
    let mut s = sender(32, 4);
    let t = transfer_number(enqueue(&mut s, bundle(200)).unwrap());
    assert!(s.cancel(SendId::Transfer(t)));
    assert!(!s.is_transfer_outstanding(t));
    assert!(s.is_window_available());
    assert!(!s.has_pending());
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn cancel_after_partial_emission_discards_the_rest_and_queues_a_cancel() {
    let mut s = sender(32, 4);
    let t = transfer_number(enqueue(&mut s, bundle(200)).unwrap());
    // Emit the first PDU: segment 0 has left.
    let first = decode_all(s.next_pdu().unwrap().data);
    assert!(matches!(
        first.as_slice(),
        [Message::TransferSegment(m), ..] if m.segment_index == 0
    ));

    assert!(s.cancel(SendId::Transfer(t)));
    assert!(!s.is_transfer_outstanding(t));
    // The remaining segments are gone; only the Cancel (plus padding) is
    // emitted.
    let pdus = drain(&mut s);
    assert_eq!(pdus.len(), 1);
    assert_eq!(
        decode_all(pdus[0].clone()),
        vec![
            cancel(t),
            Message::DefinitePadding {
                len: 32 - (HEADER_SIZE + 4) - HEADER_SIZE
            },
        ]
    );
}

#[test]
fn bogus_cancel_is_a_noop() {
    let mut s = sender(64, 4);
    let mut untouched = sender(64, 4);
    let t = transfer_number(enqueue(&mut s, bundle(200)).unwrap());
    enqueue(&mut untouched, bundle(200)).unwrap();

    // A never-allocated number: nothing queued, nothing released, so the
    // drain is exactly that of a sender never asked to cancel.
    assert!(!s.cancel(SendId::Transfer(999)));
    assert!(s.is_transfer_outstanding(t));
    assert_eq!(drain(&mut s), drain(&mut untouched));

    // Fully emitted, so no longer outstanding: cancelling it now is a
    // no-op too.
    assert!(!s.is_transfer_outstanding(t));
    assert!(!s.cancel(SendId::Transfer(t)));
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn first_segment_has_bundle_length_hint() {
    let mut s = sender(32, 16);
    enqueue(&mut s, Bytes::from(vec![0xCC; 80])).unwrap();
    let msgs = decode_all(s.next_pdu().unwrap().data);
    let Some(Message::TransferSegment(seg)) = msgs.first() else {
        panic!("expected a leading Transfer Segment, got {msgs:?}");
    };
    assert_eq!(seg.segment_index, 0);
    assert_eq!(seg.hints, vec![HintItem::BundleLength(80)]);
}

#[test]
fn variable_link_pdus_are_not_padded() {
    let mut s = variable_sender(256, BundleFraming::Message);
    let data = Bytes::from_static(b"hello");
    assert_eq!(enqueue(&mut s, data.clone()), Ok(SendId::Message(0)));

    let pdu = s.next_pdu().unwrap().data;
    assert_eq!(pdu.len(), HEADER_SIZE + data.len());
    assert_eq!(decode_all(pdu), vec![bundle_with(vec![], data)]);
}

#[test]
fn variable_link_segmented_pdus_fill_the_pdu_except_the_last() {
    let mut s = variable_sender(64, BundleFraming::Message);
    enqueue(&mut s, bundle(150)).unwrap();
    let pdus = drain(&mut s);
    let (last, full) = pdus.split_last().unwrap();
    assert!(full.iter().all(|pdu| pdu.len() == 64));

    // The last PDU is its End and nothing else: no padding.
    let [Message::TransferEnd(end)] = &decode_all(last.clone())[..] else {
        panic!("expected a lone Transfer End");
    };
    assert_eq!(last.len(), HEADER_SIZE + SEGMENT_FIELDS + end.data.len());
    let data: usize = pdus
        .iter()
        .flat_map(|pdu| decode_all(pdu.clone()))
        .map(|m| match m {
            Message::TransferSegment(m) | Message::TransferEnd(m) => m.data.len(),
            other => panic!("unexpected {other:?}"),
        })
        .sum();
    assert_eq!(data, 150);
}

#[test]
fn bare_framing_emits_the_bundle_bytes_alone_using_the_whole_pdu() {
    let pdu_size = 64;
    let mut s = variable_sender(pdu_size, BundleFraming::Bare);
    // Exactly pdu_size bytes: too big for a Bundle Message (which needs
    // a header) but it fits as a bare frame, so it is not segmented.
    let bundle = bpv7_like(pdu_size);
    assert_eq!(enqueue(&mut s, bundle.clone()), Ok(SendId::Bare(0)));

    let pdu = s.next_pdu().unwrap().data;
    assert_eq!(pdu, bundle);
    // Shared with the enqueued buffer, not copied.
    assert_eq!(pdu.as_ptr(), bundle.as_ptr());
    assert!(!s.has_pending());
    // The receive side sees it as a bundle.
    assert_eq!(decode_all(pdu), vec![bundle_with(vec![], bundle)]);
}

#[test]
fn bare_frames_keep_queue_order_and_never_share_a_pdu() {
    let mut s = variable_sender(64, BundleFraming::Bare);
    // A segmented transfer, then a bare frame, then a hinted (so framed)
    // bundle: the bare frame must neither jump the transfer nor be
    // packed with the Bundle Message behind it.
    transfer_number(enqueue(&mut s, bpv7_like(100)).unwrap());
    let bare = bpv7_like(10);
    assert_eq!(enqueue(&mut s, bare.clone()), Ok(SendId::Bare(0)));
    let hint = unknown_hint(0x41, b"z");
    assert_eq!(
        s.enqueue(
            bpv7_like(10),
            SendOptions {
                hints: vec![hint.clone()].into()
            }
        ),
        Ok(SendId::Message(1))
    );

    let pdus = drain(&mut s);
    let bare_at = pdus.iter().position(|p| *p == bare).unwrap();
    assert!(bare_at >= 1, "bare frame overtook the transfer's segments");
    assert_eq!(pdus.len(), bare_at + 2);
    // Everything before the bare frame is the transfer, and decodes
    // cleanly (the bare bytes appended to any of them would be a
    // framing fault).
    let mut ends = 0;
    for pdu in &pdus[..bare_at] {
        for msg in decode_all(pdu.clone()) {
            match msg {
                Message::TransferSegment(_) => {}
                Message::TransferEnd(_) => ends += 1,
                other => panic!("unexpected message before the bare frame: {other:?}"),
            }
        }
    }
    assert_eq!(ends, 1);
    // The framed bundle follows in a PDU of its own, unpadded.
    let last = &pdus[bare_at + 1];
    assert_eq!(
        last.len(),
        HEADER_SIZE + encoded_hints_len(from_ref(&hint)) + 10
    );
    assert_eq!(
        decode_all(last.clone()),
        vec![bundle_with(vec![hint], bpv7_like(10))]
    );
}

#[test]
fn bare_framing_keeps_hinted_bundles_framed() {
    let mut s = variable_sender(64, BundleFraming::Bare);
    let hint = unknown_hint(0x41, b"z");
    assert_eq!(
        s.enqueue(
            bpv7_like(10),
            SendOptions {
                hints: vec![hint.clone()].into(),
            },
        ),
        Ok(SendId::Message(0))
    );
    assert_eq!(
        decode_all(s.next_pdu().unwrap().data),
        vec![bundle_with(vec![hint], bpv7_like(10))]
    );
}

#[test]
fn bare_framing_requires_a_bundle_reserved_first_byte() {
    let mut s = variable_sender(64, BundleFraming::Bare);
    // 0x00 is a BTP-U message-type byte, not a bundle: a receiver could
    // not tell the bare frame from a PDU, so the bundle is framed.
    let data = Bytes::from_static(b"\x00not-a-bundle");
    assert_eq!(enqueue(&mut s, data.clone()), Ok(SendId::Message(0)));

    let pdu = s.next_pdu().unwrap().data;
    assert_eq!(frame_kind(&pdu), FrameKind::BtpuPdu);
    assert_eq!(decode_all(pdu), vec![bundle_with(vec![], data)]);
}

#[test]
fn bare_bundles_count_against_send_queue_depth() {
    let mut s = sender_with_queue_depth(
        64,
        1,
        LinkFraming::Variable {
            bundle_framing: BundleFraming::Bare,
        },
    );
    assert!(!s.is_send_queue_full());
    assert_eq!(enqueue(&mut s, bpv7_like(10)), Ok(SendId::Bare(0)));
    assert!(s.is_send_queue_full());
    s.next_pdu().unwrap();
    assert!(!s.is_send_queue_full());
}

#[test]
fn caller_hint_of_the_bundle_length_type_is_discarded_whatever_its_shape() {
    // The sender owns the Bundle Length hint.  A caller item of type 0 that
    // arrived as `Unknown` (a value length Section 9.1 does not allow, or
    // one built by hand) would otherwise ride behind the derived hint and
    // supersede it at the receiver.
    let mut s = sender(64, 16);
    s.enqueue(
        bundle(200),
        SendOptions {
            hints: vec![unknown_hint(0, &[0, 0, 0, 1])].into(),
        },
    )
    .unwrap();
    let first = decode_all(s.next_pdu().unwrap().data);
    let Message::TransferSegment(m) = &first[0] else {
        panic!("expected a segment");
    };
    assert_eq!(m.hints, vec![HintItem::BundleLength(200)]);
}

#[test]
fn a_segmented_transfer_is_one_queue_entry_until_its_end_is_packed() {
    let mut s = sender_with_queue_depth(64, 1, LinkFraming::FixedSize);
    assert!(!s.is_send_queue_full());
    transfer_number(enqueue(&mut s, bundle(200)).unwrap());
    assert!(s.is_send_queue_full());
    // Packing some of the transfer's segments does not free the entry.
    s.next_pdu().unwrap();
    assert!(s.has_pending());
    assert!(s.is_send_queue_full());
    drain(&mut s);
    assert!(!s.is_send_queue_full());
}

#[test]
fn cancel_leaves_queued_bare_bundles_intact() {
    let mut s = variable_sender(32, BundleFraming::Bare);
    let t = transfer_number(enqueue(&mut s, bpv7_like(100)).unwrap());
    let bare = bpv7_like(10);
    enqueue(&mut s, bare.clone()).unwrap();
    // Emit segment 0 so the cancel has to send a Cancel message.
    s.next_pdu().unwrap();
    assert!(s.cancel(SendId::Transfer(t)));

    // The queue is now [Cancel(t), bare frame], and the Cancel is packed
    // without the frame.
    assert_eq!(decode_all(s.next_pdu().unwrap().data), vec![cancel(t)]);
    assert_eq!(s.next_pdu().unwrap().data, bare);
    assert!(!s.has_pending());
}

// Drain `s`, keeping only the bundles each PDU carries.
fn drain_carried(s: &mut Sender) -> Vec<Vec<Carried>> {
    let mut carried = Vec::new();
    while let Some(pdu) = s.next_pdu() {
        carried.push(pdu.carried.to_vec());
    }
    carried
}

#[test]
fn pdu_lists_every_bundle_it_carries_and_flags_those_it_completes() {
    let mut s = sender(64, 4);
    // Size A so its End carries 20 bytes: 32 bytes of framing and data,
    // leaving room for B and D (14 bytes each) in the same PDU.
    let hints_len = encoded_hints_len(&[HintItem::BundleLength(0x40)]);
    let a_len = 64 - HEADER_SIZE - SEGMENT_FIELDS - hints_len + 20;
    assert_eq!(
        encoded_hints_len(&[HintItem::BundleLength(a_len as u64)]),
        hints_len
    );
    let a = enqueue(&mut s, bundle(a_len)).unwrap();
    let b = enqueue(&mut s, bundle(10)).unwrap();
    let d = enqueue(&mut s, bundle(10)).unwrap();
    // A transfer's first segment fills its PDU, so C starts a new one.
    let c = enqueue(&mut s, bundle(100)).unwrap();
    assert_eq!(
        [a, b, d, c],
        [
            SendId::Transfer(0),
            SendId::Message(0),
            SendId::Message(1),
            SendId::Transfer(1),
        ]
    );

    assert_eq!(
        drain_carried(&mut s),
        vec![
            vec![carried(a, false)],
            vec![carried(a, true), carried(b, true), carried(d, true)],
            vec![carried(c, false)],
            vec![carried(c, true)],
        ]
    );
}

#[test]
fn middle_segments_list_their_transfer_as_incomplete() {
    let mut s = sender(64, 4);
    let x = enqueue(&mut s, bundle(150)).unwrap();
    assert_eq!(
        drain_carried(&mut s),
        vec![
            vec![carried(x, false)],
            vec![carried(x, false)],
            vec![carried(x, true)],
        ]
    );
}

#[test]
fn bare_frame_pdu_lists_its_bundle_as_complete() {
    let mut s = variable_sender(64, BundleFraming::Bare);
    let id = enqueue(&mut s, bpv7_like(10)).unwrap();
    assert_eq!(id, SendId::Bare(0));
    assert_eq!(drain_carried(&mut s), vec![vec![carried(id, true)]]);
}

#[test]
fn cancelled_transfer_is_never_listed_as_complete() {
    // Unpadded, so the Cancel's PDU decodes to the Cancel alone.
    let mut s = variable_sender(64, BundleFraming::Message);
    let x = enqueue(&mut s, bundle(150)).unwrap();
    assert_eq!(s.next_pdu().unwrap().carried[..], [carried(x, false)]);
    assert!(s.cancel(x));
    // The PDU holding only the Transfer Cancel carries no bundle.
    let pdu = s.next_pdu().unwrap();
    assert_eq!(decode_all(pdu.data), vec![cancel(0)]);
    assert!(pdu.carried.is_empty());
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn next_pdu_into_replaces_the_callers_list() {
    let mut s = sender(64, 4);
    let x = enqueue(&mut s, bundle(100)).unwrap();
    let mut bundles = CarriedList::new();

    assert_eq!(s.next_pdu_into(&mut bundles).map(|pdu| pdu.len()), Some(64));
    assert_eq!(bundles[..], [carried(x, false)]);
    assert_eq!(s.next_pdu_into(&mut bundles).map(|pdu| pdu.len()), Some(64));
    assert_eq!(bundles[..], [carried(x, true)]);

    assert_eq!(s.next_pdu_into(&mut bundles), None);
    assert!(bundles.is_empty());
}

// Enqueue `n` one-byte bundles, which share a 64-byte PDU.
fn enqueue_tiny(s: &mut Sender, n: usize) -> Vec<SendId> {
    (0..n).map(|_| enqueue(s, bundle(1)).unwrap()).collect()
}

#[test]
fn next_pdu_list_moves_to_the_heap_only_past_the_inline_entries() {
    let mut s = sender(64, 4);
    enqueue_tiny(&mut s, CarriedList::INLINE);
    let pdu = s.next_pdu().unwrap();
    assert_eq!(pdu.carried.len(), CarriedList::INLINE);
    assert_eq!(pdu.carried.capacity(), CarriedList::INLINE);

    let ids = enqueue_tiny(&mut s, CarriedList::INLINE + 1);
    let pdu = s.next_pdu().unwrap();
    let expected: Vec<_> = ids.into_iter().map(|id| carried(id, true)).collect();
    assert_eq!(pdu.carried[..], expected[..]);
    assert!(pdu.carried.capacity() > CarriedList::INLINE);
}

#[test]
fn reused_list_keeps_its_heap_buffer_for_a_pdu_that_would_fit_inline() {
    let mut s = sender(64, 4);
    let mut bundles = CarriedList::new();
    enqueue_tiny(&mut s, CarriedList::INLINE + 1);
    s.next_pdu_into(&mut bundles).unwrap();
    let grown = bundles.capacity();
    assert!(grown > CarriedList::INLINE);

    let x = enqueue(&mut s, bundle(10)).unwrap();
    s.next_pdu_into(&mut bundles).unwrap();
    assert_eq!(bundles[..], [carried(x, true)]);
    assert_eq!(bundles.capacity(), grown);
    // Cleared by an empty queue, the buffer still stays.
    assert_eq!(s.next_pdu_into(&mut bundles), None);
    assert_eq!(bundles.capacity(), grown);
}

#[test]
fn with_capacity_below_the_inline_entries_stays_inline() {
    assert_eq!(
        CarriedList::with_capacity(0).capacity(),
        CarriedList::INLINE
    );
    assert_eq!(
        CarriedList::with_capacity(CarriedList::INLINE).capacity(),
        CarriedList::INLINE
    );
    assert!(CarriedList::with_capacity(CarriedList::INLINE + 1).capacity() > CarriedList::INLINE);
}

#[test]
fn list_sized_by_the_bundle_bound_holds_the_fullest_pdu_of_valid_bundles() {
    // The smallest valid BPv7 bundle, 32 bytes as a Bundle Message.
    const MIN_BUNDLE: usize = 28;
    // A 13-byte Transfer End and six such messages fill this PDU exactly.
    const PDU: usize = 13 + 6 * (HEADER_SIZE + MIN_BUNDLE);
    let bound = PDU / 32 + 1;
    let mut s = sender(PDU, 4);

    // Size A so its End, after two full segments, carries one byte.  (A
    // two-segment End carries at least 12, since only a bundle too long
    // for a Bundle Message is segmented.)
    let hints_len = encoded_hints_len(&[HintItem::BundleLength(2 * PDU as u64)]);
    let a_len =
        (PDU - HEADER_SIZE - SEGMENT_FIELDS - hints_len) + (PDU - HEADER_SIZE - SEGMENT_FIELDS) + 1;
    assert_eq!(
        encoded_hints_len(&[HintItem::BundleLength(a_len as u64)]),
        hints_len
    );
    enqueue(&mut s, bundle(a_len)).unwrap();
    for _ in 0..2 * bound {
        enqueue(&mut s, bundle(MIN_BUNDLE)).unwrap();
    }

    let mut bundles = CarriedList::with_capacity(bound);
    let capacity = bundles.capacity();
    let mut lens = Vec::new();
    while s.next_pdu_into(&mut bundles).is_some() {
        lens.push(bundles.len());
    }
    // The End's PDU reaches the bound; one of messages alone holds
    // `PDU / 32`.
    assert_eq!(lens[..4], [1, 1, bound, PDU / 32]);
    assert_eq!(bundles.capacity(), capacity);
}

#[test]
fn lists_compare_by_entries_not_storage() {
    let (mut a, mut b) = (sender(64, 4), sender(64, 4));
    let x = enqueue(&mut a, bundle(10)).unwrap();
    assert_eq!(enqueue(&mut b, bundle(10)).unwrap(), x);

    let mut heap = CarriedList::with_capacity(2 * CarriedList::INLINE);
    a.next_pdu_into(&mut heap).unwrap();
    let inline = b.next_pdu().unwrap().carried;
    assert!(heap.capacity() > inline.capacity());

    assert_eq!(heap, inline);
    let hash = |list: &CarriedList| {
        let mut h = DefaultHasher::new();
        list.hash(&mut h);
        h.finish()
    };
    assert_eq!(hash(&heap), hash(&inline));
    let expected = format!("{:?}", [carried(x, true)]);
    assert_eq!(format!("{heap:?}"), expected);
    assert_eq!(format!("{inline:?}"), expected);
}

#[test]
fn cancel_goes_ahead_of_the_queued_backlog() {
    // Unpadded, so each PDU decodes to its messages alone.
    let mut s = variable_sender(64, BundleFraming::Message);
    let x = enqueue(&mut s, bundle(150)).unwrap();
    let y = enqueue(&mut s, bundle(150)).unwrap();
    let b = enqueue(&mut s, bundle(10)).unwrap();
    assert_eq!(s.next_pdu().unwrap().carried[..], [carried(x, false)]);

    assert!(s.cancel(x));
    // Y's first segment fills a PDU, so the Cancel travels alone rather
    // than behind Y's three PDUs and B's one.
    assert_eq!(decode_all(s.next_pdu().unwrap().data), vec![cancel(0)]);
    assert_eq!(
        drain_carried(&mut s),
        vec![
            vec![carried(y, false)],
            vec![carried(y, false)],
            vec![carried(y, true)],
            vec![carried(b, true)],
        ]
    );
}

#[test]
fn cancel_removes_a_queued_bundle_message_and_sends_nothing_for_it() {
    let mut s = variable_sender(64, BundleFraming::Message);
    let a = enqueue(&mut s, bundle(10)).unwrap();
    let b = enqueue(&mut s, bundle(20)).unwrap();
    assert!(s.cancel(a));
    // Gone from the queue, so a second cancel finds nothing.
    assert!(!s.cancel(a));

    let pdu = s.next_pdu().unwrap();
    assert_eq!(decode_all(pdu.data), vec![bundle_with(vec![], bundle(20))]);
    assert_eq!(pdu.carried[..], [carried(b, true)]);
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn cancel_removes_a_queued_bare_frame() {
    let mut s = variable_sender(64, BundleFraming::Bare);
    let a = enqueue(&mut s, bpv7_like(10)).unwrap();
    let b = bpv7_like(20);
    enqueue(&mut s, b.clone()).unwrap();
    assert!(s.cancel(a));
    assert_eq!(drain(&mut s), vec![b]);
}

#[test]
fn cancel_after_the_last_bytes_are_packed_changes_nothing() {
    let mut s = variable_sender(64, BundleFraming::Bare);
    let message = enqueue(&mut s, bundle(10)).unwrap();
    s.next_pdu().unwrap();
    let bare = enqueue(&mut s, bpv7_like(10)).unwrap();
    s.next_pdu().unwrap();
    let transfer = enqueue(&mut s, bundle(100)).unwrap();
    drain(&mut s);

    for id in [message, bare, transfer] {
        assert!(!s.cancel(id), "{id:?}");
    }
    assert!(!s.has_pending());
}

#[test]
fn cancel_matches_the_id_variant_as_well_as_the_number() {
    // Message(0), Bare(1), and Transfer(0) are queued; Bare(0), Message(1),
    // and Transfer(1) name none of them.
    let mut s = variable_sender(64, BundleFraming::Bare);
    let message = enqueue(&mut s, bundle(10)).unwrap();
    let bare = enqueue(&mut s, bpv7_like(10)).unwrap();
    let transfer = enqueue(&mut s, bundle(100)).unwrap();
    assert_eq!(
        [message, bare, transfer],
        [SendId::Message(0), SendId::Bare(1), SendId::Transfer(0),]
    );
    for id in [SendId::Bare(0), SendId::Message(1), SendId::Transfer(1)] {
        assert!(!s.cancel(id), "{id:?}");
    }
    assert_eq!(
        drain_carried(&mut s),
        vec![
            vec![carried(message, true)],
            vec![carried(bare, true)],
            vec![carried(transfer, false)],
            vec![carried(transfer, true)],
        ]
    );
}

#[test]
fn cancel_by_one_variant_leaves_the_others_with_that_number() {
    // Message(0) and Transfer(0) share a number; cancelling the first must
    // not touch the second.
    let mut s = variable_sender(64, BundleFraming::Bare);
    let message = enqueue(&mut s, bundle(10)).unwrap();
    let bare = enqueue(&mut s, bpv7_like(10)).unwrap();
    let transfer = enqueue(&mut s, bundle(100)).unwrap();
    assert_eq!(
        [message, transfer],
        [SendId::Message(0), SendId::Transfer(0)]
    );

    assert!(s.cancel(message));
    assert!(s.is_transfer_outstanding(0));
    assert_eq!(
        drain_carried(&mut s),
        vec![
            vec![carried(bare, true)],
            vec![carried(transfer, false)],
            vec![carried(transfer, true)],
        ]
    );
}

#[test]
fn cancelling_a_queued_bundle_frees_send_queue_depth() {
    let mut s = sender_with_queue_depth(64, 1, LinkFraming::FixedSize);
    let id = enqueue(&mut s, bundle(10)).unwrap();
    assert!(s.is_send_queue_full());
    assert!(s.cancel(id));
    assert!(!s.is_send_queue_full());
}

#[test]
fn debug_summarises_the_queue_instead_of_printing_it() {
    let mut s = sender(64, 4);
    enqueue(&mut s, Bytes::from(vec![0xAB; 1000])).unwrap();
    enqueue(&mut s, Bytes::from_static(b"small")).unwrap();
    let depth = SendQueueDepth::DEFAULT;
    let summary = format!(
        "Sender {{ pdu_size: PduSize(64), send_queue_depth: SendQueueDepth({depth}), \
         link_framing: FixedSize, window_size: WindowSize(4), \
         transfers_outstanding: 1, window_available: true, queued: 2, next_bundle_id: 1"
    );
    #[cfg(not(feature = "tower"))]
    let expected = format!("{summary} }}");
    #[cfg(feature = "tower")]
    let expected = format!("{summary}, enqueue_wakers: 0, drain_waker: false }}");
    assert_eq!(format!("{s:?}"), expected);
}
