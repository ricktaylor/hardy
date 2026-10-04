//! Streamed delivery and `Receiver::refuse` through the public `receiver`
//! API.

mod common;

use bytes::Bytes;
use hardy_btpu::{
    codec::{
        hint::{HintItem, Hints},
        message::Message,
    },
    fec::PreAgreedFecMessage,
    receiver::{
        Delivery, DropReason, MaxSegments, Receiver, ReceiverConfig, ReceiverEvent, RejectReason,
    },
    transfer::TransferId,
};

use self::common::{
    Event, bundle_msg, cancel, cancelled, dropped, encode, end, expired, is_within, none, received,
    receiver_config, rejected, segment, segment_with, unknown_hint,
};

fn streamed_config(window: u16, cap: usize) -> ReceiverConfig {
    ReceiverConfig {
        delivery: Delivery::Streamed,
        ..receiver_config(window, cap)
    }
}

fn streamed(window: u16, cap: usize) -> Receiver {
    Receiver::new(streamed_config(window, cap))
}

fn started(transfer_number: u32, hints: Vec<HintItem>) -> Event {
    Event::TransferStarted {
        transfer_number,
        hints: Hints::from(hints),
    }
}

fn data(transfer_number: u32, data: &'static [u8]) -> Event {
    data_with(transfer_number, data, None)
}

fn data_with(transfer_number: u32, data: &'static [u8], hints: Option<Vec<HintItem>>) -> Event {
    Event::TransferData {
        transfer_number,
        data: Bytes::from_static(data),
        hints: hints.map(Hints::from),
    }
}

fn finished(transfer_number: u32, data: &'static [u8]) -> Event {
    Event::TransferFinished {
        transfer_number,
        data: Bytes::from_static(data),
        hints: None,
    }
}

// The id `events` started a transfer with.
fn started_id(events: &[ReceiverEvent]) -> TransferId {
    events
        .iter()
        .find_map(|e| match e {
            ReceiverEvent::TransferStarted { id, .. } => Some(*id),
            _ => None,
        })
        .expect("a transfer started")
}

#[test]
fn in_order_segments_are_released_as_views_of_their_pdus() {
    let mut r = streamed(16, 1024);
    // A segment this short is copied out of its PDU when held (see
    // `segments_shorter_than_half_the_pdu_are_copied_out_of_it` in
    // receiver.rs); released at once, it is not held.
    let pdu = encode(&segment(0, 0, b"abc"));
    let events = r.receive_pdu(pdu.clone());
    assert_eq!(events, vec![started(0, vec![]), data(0, b"abc")]);
    let ReceiverEvent::TransferData { data, .. } = &events[1] else {
        panic!("the second event carries the data");
    };
    assert!(is_within(data, &pdu));
    assert_eq!(r.retained_bytes(), 0);

    let pdu = encode(&end(0, 1, b"def"));
    let events = r.receive_pdu(pdu.clone());
    assert_eq!(events, vec![finished(0, b"def")]);
    let ReceiverEvent::TransferFinished { data, .. } = &events[0] else {
        panic!("the event carries the data");
    };
    assert!(is_within(data, &pdu));
}

#[test]
fn a_filled_gap_releases_the_run_behind_it() {
    let mut r = streamed(16, 1024);
    assert_eq!(r.process_message(segment(0, 1, b"b")), none());
    assert_eq!(r.process_message(segment(0, 2, b"c")), none());
    assert!(r.retained_bytes() > 0);
    assert_eq!(
        r.process_message(segment(0, 0, b"a")),
        vec![
            started(0, vec![]),
            data(0, b"a"),
            data(0, b"b"),
            data(0, b"c")
        ]
    );
    assert_eq!(r.retained_bytes(), 0);
    assert_eq!(r.process_message(end(0, 3, b"d")), vec![finished(0, b"d")]);
}

#[test]
fn an_end_arriving_before_the_gap_fills_finishes_with_the_run() {
    let mut r = streamed(16, 1024);
    assert_eq!(r.process_message(end(0, 1, b"b")), none());
    assert_eq!(
        r.process_message(segment(0, 0, b"a")),
        vec![started(0, vec![]), data(0, b"a"), finished(0, b"b")]
    );
    assert_eq!(
        r.process_message(end(0, 1, b"b")),
        vec![dropped(0, DropReason::Delivered)]
    );
}

#[test]
fn a_lone_end_starts_and_finishes() {
    let mut r = streamed(16, 1024);
    assert_eq!(
        r.process_message(end(0, 0, b"whole")),
        vec![started(0, vec![]), finished(0, b"whole")]
    );
}

#[test]
fn an_end_naming_a_released_segment_finishes_with_no_data() {
    let mut r = streamed(16, 1024);
    assert_eq!(
        r.process_message(segment(0, 0, b"a")),
        vec![started(0, vec![]), data(0, b"a")]
    );
    // The released Segment keeps its bytes; the End only records N.
    assert_eq!(r.process_message(end(0, 0, b"a")), vec![finished(0, b"")]);
}

#[test]
fn empty_segments_are_not_reported() {
    let mut r = streamed(16, 1024);
    assert_eq!(r.process_message(segment(0, 0, b"")), none());
    assert_eq!(
        r.process_message(segment(0, 1, b"a")),
        vec![started(0, vec![]), data(0, b"a")]
    );
    assert_eq!(r.process_message(segment(0, 2, b"")), none());
    assert_eq!(r.process_message(end(0, 3, b"")), vec![finished(0, b"")]);
}

#[test]
fn a_transfer_with_no_data_is_rejected_without_starting() {
    let mut r = streamed(16, 1024);
    assert_eq!(r.process_message(segment(0, 0, b"")), none());
    assert_eq!(
        r.process_message(end(0, 1, b"")),
        vec![rejected(0, RejectReason::Empty)]
    );
}

#[test]
fn a_started_transfer_can_be_cancelled() {
    let mut r = streamed(16, 1024);
    r.process_message(segment(0, 0, b"a"));
    assert_eq!(r.process_message(cancel(0)), vec![cancelled(0)]);
    assert_eq!(
        r.process_message(segment(0, 1, b"b")),
        vec![dropped(0, DropReason::Cancelled)]
    );
}

#[test]
fn a_started_transfer_can_expire() {
    let mut r = streamed(4, 1024);
    r.process_message(segment(0, 0, b"a"));
    assert_eq!(
        r.process_message(segment(4, 0, b"x")),
        vec![expired(0), started(4, vec![]), data(4, b"x")]
    );
}

#[test]
fn released_bytes_count_toward_the_transfer_size_cap() {
    let mut r = streamed(16, 4);
    assert_eq!(
        r.process_message(segment(0, 0, b"abc")),
        vec![started(0, vec![]), data(0, b"abc")]
    );
    assert_eq!(
        r.process_message(segment(0, 1, b"de")),
        vec![rejected(0, RejectReason::TooLarge)]
    );
}

#[test]
fn a_repeat_of_a_released_segment_is_a_duplicate() {
    let mut r = streamed(16, 1024);
    r.process_message(segment(0, 0, b"a"));
    assert_eq!(
        r.process_message(segment(0, 0, b"a")),
        vec![dropped(0, DropReason::Duplicate)]
    );
}

#[test]
fn an_end_below_a_released_segment_conflicts() {
    let mut r = streamed(16, 1024);
    r.process_message(segment(0, 0, b"a"));
    r.process_message(segment(0, 1, b"b"));
    assert_eq!(
        r.process_message(end(0, 0, b"a")),
        vec![dropped(0, DropReason::SegmentIndexConflict)]
    );
}

#[test]
fn both_deliveries_reject_at_the_same_segment() {
    // Released segments count toward the segment limit and the
    // bookkeeping budget, so a transfer fails at the same message whether
    // its segments were released or held.
    let configs = |max_segments| {
        [Delivery::Whole, Delivery::Streamed].map(|delivery| ReceiverConfig {
            delivery,
            max_segments_per_transfer: max_segments,
            ..receiver_config(16, 64)
        })
    };
    for max_segments in [Some(MaxSegments::try_from(3).unwrap()), None] {
        let rejected_at = configs(max_segments).map(|config| {
            let mut r = Receiver::new(config);
            (0..)
                .find(|&i| {
                    r.process_message(segment(0, i, b""))
                        .iter()
                        .any(|e| matches!(e, ReceiverEvent::TransferRejected { .. }))
                })
                .unwrap()
        });
        assert_eq!(rejected_at[0], rejected_at[1], "{max_segments:?}");
    }
}

#[test]
fn hints_are_reported_when_they_change() {
    let a = unknown_hint(0x41, b"a");
    let b = unknown_hint(0x42, b"b");
    let c = unknown_hint(0x43, b"c");
    let mut r = streamed(16, 1024);
    assert_eq!(
        r.process_message(segment_with(
            0,
            0,
            vec![a.clone()],
            Bytes::from_static(b"0")
        )),
        vec![started(0, vec![a.clone()]), data(0, b"0")]
    );
    // A repeat of a value already reported is no change.
    assert_eq!(
        r.process_message(segment_with(
            0,
            1,
            vec![a.clone()],
            Bytes::from_static(b"1")
        )),
        vec![data(0, b"1")]
    );
    assert_eq!(
        r.process_message(segment_with(
            0,
            2,
            vec![b.clone()],
            Bytes::from_static(b"2")
        )),
        vec![data_with(0, b"2", Some(vec![a.clone(), b.clone()]))]
    );
    // A change on a message that releases nothing goes out on the next
    // event.
    assert_eq!(
        r.process_message(segment_with(
            0,
            4,
            vec![c.clone()],
            Bytes::from_static(b"4")
        )),
        none()
    );
    assert_eq!(
        r.process_message(segment(0, 3, b"3")),
        vec![data_with(0, b"3", Some(vec![a, b, c])), data(0, b"4")]
    );
}

#[test]
fn hints_before_the_first_data_go_out_on_started() {
    let a = unknown_hint(0x41, b"a");
    let mut r = streamed(16, 1024);
    assert_eq!(
        r.process_message(segment_with(0, 0, vec![a.clone()], Bytes::new())),
        none()
    );
    assert_eq!(
        r.process_message(segment(0, 1, b"x")),
        vec![started(0, vec![a]), data(0, b"x")]
    );
}

#[test]
fn fec_transfers_release_nothing() {
    let mut r = Receiver::new(ReceiverConfig {
        fec: true,
        ..streamed_config(16, 1024)
    });
    let fec = Message::PreAgreedFecSource(PreAgreedFecMessage {
        transfer_number: 0,
        fec_instance_id: 1,
        hints: vec![],
        payload: Bytes::from_static(b"fec"),
    });
    assert_eq!(r.process_message(fec), none());
}

#[test]
fn bundle_messages_are_received_whole() {
    let mut r = streamed(16, 1024);
    assert_eq!(
        r.process_message(bundle_msg(b"hello")),
        vec![received(b"hello")]
    );
}

#[test]
fn refuse_closes_a_held_transfer() {
    let mut r = streamed(16, 1024);
    let id = started_id(&r.process_message(segment(0, 0, b"a")));
    r.process_message(segment(0, 2, b"c"));
    assert!(r.retained_bytes() > 0);

    assert!(r.refuse(id));
    assert_eq!(r.retained_bytes(), 0);
    assert_eq!(
        r.process_message(segment(0, 1, b"b")),
        vec![dropped(0, DropReason::Refused)]
    );
    assert!(!r.refuse(id));
}

#[test]
fn ids_from_two_receivers_never_compare_equal() {
    let (mut r, mut other) = (streamed(16, 1024), streamed(16, 1024));
    let id = started_id(&r.process_message(segment(0, 0, b"a")));
    let twin = started_id(&other.process_message(segment(0, 0, b"a")));
    assert_eq!(id.transfer_number(), twin.transfer_number());
    assert_ne!(id, twin);
}

#[test]
#[cfg_attr(
    debug_assertions,
    should_panic(expected = "issued by another Receiver")
)]
fn another_receivers_id_refuses_nothing() {
    // A debug build stops at the foreign id; a release build ignores it.
    let (mut r, mut other) = (streamed(16, 1024), streamed(16, 1024));
    r.process_message(segment(0, 0, b"a"));
    r.process_message(segment(0, 2, b"c"));
    let twin = started_id(&other.process_message(segment(0, 0, b"a")));
    let retained = r.retained_bytes();

    assert!(!r.refuse(twin));
    assert_eq!(r.retained_bytes(), retained);
    assert_eq!(
        r.process_message(segment(0, 1, b"b")),
        vec![data(0, b"b"), data(0, b"c")]
    );
}

#[test]
fn refuse_ignores_a_finished_transfer() {
    let mut r = streamed(16, 1024);
    let id = started_id(&r.process_message(end(0, 0, b"a")));
    assert!(!r.refuse(id));
    assert_eq!(
        r.process_message(end(0, 0, b"a")),
        vec![dropped(0, DropReason::Delivered)]
    );
}

#[test]
fn refuse_ignores_an_expired_transfer() {
    let mut r = streamed(4, 1024);
    let id = started_id(&r.process_message(segment(0, 0, b"a")));
    r.process_message(segment(4, 0, b"x"));
    assert!(!r.refuse(id));
}

#[test]
fn refuse_ignores_an_id_from_before_a_reset() {
    let mut r = streamed(16, 1024);
    let before = started_id(&r.process_message(segment(0, 0, b"a")));
    r.reset();
    let after = started_id(&r.process_message(segment(0, 0, b"a")));
    assert_ne!(before, after);
    assert_eq!(before.transfer_number(), after.transfer_number());

    assert!(!r.refuse(before));
    assert_eq!(r.process_message(end(0, 1, b"b")), vec![finished(0, b"b")]);
}

#[test]
fn a_dropped_message_names_its_transfer_by_id_inside_the_window() {
    let mut r = streamed(4, 1024);
    let id = started_id(&r.process_message(segment(0, 0, b"a")));
    let drop = |transfer_number, id, reason| ReceiverEvent::MessageDropped {
        transfer_number,
        id,
        reason,
    };

    assert_eq!(
        r.process_message(segment(0, 0, b"a")),
        vec![drop(0, Some(id), DropReason::Duplicate)]
    );
    assert_eq!(r.process_message(end(0, 1, b"b")), vec![finished(0, b"b")]);
    assert_eq!(
        r.process_message(end(0, 1, b"b")),
        vec![drop(0, Some(id), DropReason::Delivered)]
    );
    // No id is assigned to a number outside the window.
    assert_eq!(
        r.process_message(cancel(100)),
        vec![drop(100, None, DropReason::UnknownTransfer)]
    );
}
