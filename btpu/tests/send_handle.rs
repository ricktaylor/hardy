//! Bundles pushed in chunks through `Sender::begin`, `push`, and `finish`.

mod common;

use bytes::Bytes;
use hardy_btpu::{
    codec::{
        header::HEADER_SIZE,
        hint::{HintItem, encoded_hints_len},
        message::Message,
    },
    sender::{
        BundleFraming, CarriedList, Error, LinkFraming, NextPduOptions, SegmentCutStrategy,
        SendHandle, SendId, SendKind, SendOptions, Sender,
    },
    transfer::Error as TransferError,
};

use self::common::{
    SEGMENT_FIELDS, bpv7_like, bundle, bundle_msg, bundle_with, cancel, carried, decode_all, drain,
    enqueue, first_capacity, patterned, segment_with, sender, sender_with,
    sender_with_high_watermark, transfer_data, variable_sender, window_size,
};

fn begin(s: &mut Sender, total_len: usize) -> SendHandle {
    s.begin(total_len, SendOptions::default()).unwrap()
}

// Push `data` through `handle` in chunks of `chunk` bytes.
fn push_in_chunks(s: &mut Sender, handle: &mut SendHandle, data: &Bytes, chunk: usize) {
    for start in (0..data.len()).step_by(chunk) {
        let end = (start + chunk).min(data.len());
        s.push(handle, data.slice(start..end)).unwrap();
    }
}

#[test]
fn a_pushed_bundle_goes_out_as_the_same_bundle_enqueued() {
    for len in [10, 200] {
        let data = patterned(len);
        let mut pushed = sender(64, 16);
        let mut handle = begin(&mut pushed, len);
        push_in_chunks(&mut pushed, &mut handle, &data, 7);
        let id = pushed.finish(handle).unwrap();

        let mut enqueued = sender(64, 16);
        assert_eq!(
            enqueue(&mut enqueued, data).map(SendId::kind),
            Ok(id.kind())
        );
        assert_eq!(drain(&mut pushed), drain(&mut enqueued));
    }
}

#[test]
fn another_senders_handle_is_refused_and_changes_nothing() {
    // Both senders number from 0, so each handle has a twin of the same
    // kind and number in the other sender.
    let (mut a, mut b) = (sender(64, 16), sender(64, 16));
    let mut fitting = begin(&mut a, 10);
    let mut segmented = begin(&mut a, 200);
    let twins = [begin(&mut b, 10), begin(&mut b, 200)];
    for handle in [&mut fitting, &mut segmented] {
        assert_eq!(
            b.push(handle, Bytes::from_static(b"x")),
            Err(Error::ForeignHandle)
        );
        assert_eq!(handle.pushed(), 0);
    }
    assert_eq!((a.queued_bytes(), b.queued_bytes()), (0, 0));

    let ids = [fitting.id(), segmented.id()];
    assert_eq!(b.finish(fitting), Err(Error::ForeignHandle));
    assert_eq!(b.finish(segmented), Err(Error::ForeignHandle));
    for (id, twin) in ids.into_iter().zip(&twins) {
        assert!(a.is_outstanding(id));
        assert!(b.is_outstanding(twin.id()));
    }
}

#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "issued by another Sender"))]
fn another_senders_handle_cancels_nothing() {
    let (mut a, mut b) = (sender(64, 16), sender(64, 16));
    let foreign = begin(&mut a, 200);
    let own = begin(&mut b, 200);
    assert_eq!(foreign.id().kind(), own.id().kind());
    let own_id = own.id();
    assert!(!b.cancel(foreign));
    assert!(b.is_outstanding(own_id));
}

// The bytes of every Bundle Message in `pdus`, in order.
fn bundle_messages(pdus: &[Bytes]) -> Vec<Bytes> {
    pdus.iter()
        .flat_map(|pdu| decode_all(pdu.clone()))
        .filter_map(|m| match m {
            Message::Bundle { data, .. } => Some(data),
            _ => None,
        })
        .collect()
}

#[test]
fn each_sender_completes_its_own_bundles_beside_refused_foreign_pushes() {
    // Each handle has a twin of the same kind and number in the other
    // sender, with a different length and different bytes, so a push that
    // reached the wrong bundle would show in either sender's output.
    let (mut a, mut b) = (sender(64, 16), sender(64, 16));
    let a_data = [patterned(10), patterned(200)];
    let b_data = [bundle(12), bundle(300)];
    let mut a_handles = a_data.clone().map(|d| begin(&mut a, d.len()));
    let mut b_handles = b_data.clone().map(|d| begin(&mut b, d.len()));
    for handle in &mut a_handles {
        assert_eq!(b.push(handle, bundle(1)), Err(Error::ForeignHandle));
    }
    for handle in &mut b_handles {
        assert_eq!(a.push(handle, patterned(1)), Err(Error::ForeignHandle));
    }

    for (s, handles, data) in [(&mut a, a_handles, &a_data), (&mut b, b_handles, &b_data)] {
        for (mut handle, data) in handles.into_iter().zip(data) {
            s.push(&mut handle, data.clone()).unwrap();
            s.finish(handle).unwrap();
        }
        let pdus = drain(s);
        assert_eq!(bundle_messages(&pdus), [data[0].clone()]);
        assert_eq!(transfer_data(&pdus, 0), data[1]);
        assert_eq!(s.queued_bytes(), 0);
    }
}

#[test]
fn a_fitting_bundle_is_queued_when_its_last_byte_is_pushed() {
    let mut s = sender(64, 16);
    let mut handle = begin(&mut s, 10);
    let id = handle.id();
    assert_eq!(id.kind(), SendKind::Message);
    s.push(&mut handle, Bytes::from_static(b"0123")).unwrap();
    assert_eq!(s.queued_bytes(), 4);
    assert_eq!(s.next_pdu(), None);

    s.push(&mut handle, Bytes::from_static(b"456789")).unwrap();
    assert_eq!(s.finish(handle), Ok(id));
    let pdu = s.next_pdu().unwrap();
    assert_eq!(&pdu.carried[..], &[carried(id, true)]);
    assert_eq!(decode_all(pdu.data)[0], bundle_msg(b"0123456789"));
    assert_eq!(s.queued_bytes(), 0);
}

#[test]
fn a_segment_goes_out_once_its_bytes_are_pushed() {
    let data = patterned(200);
    let mut s = sender(64, 16);
    let mut handle = begin(&mut s, data.len());
    assert!(s.has_pending());
    assert_eq!(s.next_pdu(), None);

    // Segment 0 carries the Bundle Length hint, so fewer than 64 bytes
    // fill it.
    s.push(&mut handle, data.slice(..64)).unwrap();
    let first = s.next_pdu().unwrap();
    assert_eq!(&first.carried[..], &[carried(handle.id(), false)]);
    assert_eq!(s.next_pdu(), None);
    assert!(s.queued_bytes() < 64);

    s.push(&mut handle, data.slice(64..)).unwrap();
    s.finish(handle).unwrap();
    let mut pdus = vec![first.data];
    pdus.extend(drain(&mut s));
    let mut enqueued = sender(64, 16);
    enqueue(&mut enqueued, data).unwrap();
    assert_eq!(pdus, drain(&mut enqueued));
}

#[test]
fn an_overrun_is_refused_and_leaves_the_bundle_as_it_was() {
    let mut s = sender(64, 16);
    let mut handle = begin(&mut s, 10);
    let id = handle.id();
    s.push(&mut handle, Bytes::from_static(b"01234")).unwrap();
    assert_eq!(
        s.push(&mut handle, Bytes::from_static(b"567890")),
        Err(Error::Overrun {
            total_len: 10,
            pushed: 5,
            chunk: 6,
        })
    );
    assert_eq!((handle.pushed(), s.queued_bytes()), (5, 5));
    s.push(&mut handle, Bytes::from_static(b"56789")).unwrap();
    assert_eq!(s.finish(handle), Ok(id));
    assert_eq!(id.kind(), SendKind::Message);
    assert_eq!(
        decode_all(s.next_pdu().unwrap().data)[0],
        bundle_msg(b"0123456789")
    );
}

#[test]
fn an_empty_chunk_does_nothing() {
    let mut s = sender(64, 16);
    let mut handle = begin(&mut s, 3);
    let id = handle.id();
    s.push(&mut handle, Bytes::new()).unwrap();
    assert_eq!((handle.pushed(), s.queued_bytes()), (0, 0));
    s.push(&mut handle, Bytes::from_static(b"abc")).unwrap();
    // Even once the bundle is complete.
    s.push(&mut handle, Bytes::new()).unwrap();
    assert_eq!(s.finish(handle), Ok(id));
    assert_eq!(id.kind(), SendKind::Message);
}

#[test]
fn an_underrun_at_finish_cancels_a_started_transfer() {
    let data = patterned(200);
    let mut s = sender(64, 16);
    let mut handle = begin(&mut s, data.len());
    let id = handle.id();
    s.push(&mut handle, data.slice(..100)).unwrap();
    s.next_pdu().unwrap();

    assert_eq!(
        s.finish(handle),
        Err(Error::Underrun {
            total_len: 200,
            pushed: 100,
        })
    );
    assert!(!s.is_outstanding(id));
    assert_eq!(s.queued_bytes(), 0);
    // The sender's first transfer number.
    assert_eq!(decode_all(s.next_pdu().unwrap().data)[0], cancel(0));
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn an_underrun_at_finish_drops_a_partly_pushed_fitting_bundle() {
    let mut s = sender(64, 16);
    let mut handle = begin(&mut s, 10);
    s.push(&mut handle, Bytes::from_static(b"0123")).unwrap();
    assert_eq!(
        s.finish(handle),
        Err(Error::Underrun {
            total_len: 10,
            pushed: 4,
        })
    );
    assert_eq!(s.queued_bytes(), 0);
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn cancelling_a_handle_before_any_push_sends_nothing() {
    let mut s = sender(64, 16);
    let fitting = begin(&mut s, 10);
    let segmented = begin(&mut s, 200);
    let id = segmented.id();
    assert!(s.cancel(fitting));
    assert!(s.cancel(segmented));
    assert!(!s.is_outstanding(id));
    assert!(!s.has_pending());
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn cancelling_a_handle_drops_its_pushed_bytes() {
    let data = patterned(200);
    let mut s = sender(64, 16);
    let mut fitting = begin(&mut s, 10);
    let mut segmented = begin(&mut s, data.len());
    s.push(&mut fitting, Bytes::from_static(b"01234")).unwrap();
    s.push(&mut segmented, data.slice(..30)).unwrap();
    assert_eq!(s.queued_bytes(), 35);
    assert!(s.cancel(fitting));
    assert_eq!(s.queued_bytes(), 30);
    assert!(s.cancel(segmented));
    assert_eq!(s.queued_bytes(), 0);
    // Nothing of the transfer had been emitted, so no Cancel goes out.
    assert_eq!(s.next_pdu(), None);
}

#[test]
fn a_dropped_handle_leaves_its_bundle_outstanding_until_cancelled_by_id() {
    let mut s = sender(32, 16);
    let mut segmented = begin(&mut s, 200);
    let mut fitting = begin(&mut s, 10);
    s.push(&mut segmented, bundle(200).slice(..17)).unwrap();
    s.push(&mut fitting, Bytes::from_static(b"01234")).unwrap();
    let first = s.next_pdu().unwrap();
    assert_eq!(&first.carried[..], &[carried(segmented.id(), false)]);
    let queued = enqueue(&mut s, bundle(10)).unwrap();

    // The handles are gone; their bundles are listed in queue order, the
    // one still being assembled last.
    let ids = [segmented.id(), queued, fitting.id()];
    drop(segmented);
    drop(fitting);
    assert_eq!(s.outstanding().collect::<Vec<_>>(), ids);
    assert!(ids.iter().all(|&id| s.is_outstanding(id)));
    assert_eq!(s.queued_bytes(), 15);

    // Cancelling by ID frees them, and the receiver is told about the
    // transfer it has started.
    for id in ids {
        assert!(s.cancel(id));
    }
    assert_eq!(s.outstanding().next(), None);
    assert_eq!(s.queued_bytes(), 0);
    let pdu = s.next_pdu().unwrap();
    assert_eq!(decode_all(pdu.data)[0], cancel(0));
    assert_eq!(s.next_pdu(), None);
}

#[cfg(target_pointer_width = "64")]
#[test]
fn begin_refuses_a_length_whose_framing_would_overflow() {
    let mut s = sender(64, 16);
    for len in usize::MAX - HEADER_SIZE - 8..=usize::MAX {
        assert_eq!(
            s.begin(len, SendOptions::default()).map(|h| h.id()),
            Err(Error::TooManySegments { len, pdu_size: 64 })
        );
    }
    assert_eq!(s.outstanding().next(), None);
    assert!(s.is_window_available());
}

#[test]
fn a_push_after_cancelling_by_id_is_not_in_progress() {
    let mut s = sender(64, 16);
    let mut fitting = begin(&mut s, 10);
    let mut segmented = begin(&mut s, 200);
    assert!(s.cancel(fitting.id()));
    assert!(s.cancel(segmented.id()));
    for handle in [&mut fitting, &mut segmented] {
        assert_eq!(
            s.push(handle, Bytes::from_static(b"x")),
            Err(Error::NotInProgress)
        );
        assert_eq!(handle.pushed(), 0);
    }
    assert_eq!(s.queued_bytes(), 0);
}

#[test]
fn a_bare_bundle_must_start_with_a_bundle_reserved_byte() {
    let mut s = variable_sender(64, BundleFraming::Bare);
    let mut handle = begin(&mut s, 20);
    assert_eq!(handle.id().kind(), SendKind::Bare);
    assert_eq!(
        s.push(&mut handle, Bytes::from_static(&[0x01; 20])),
        Err(Error::NotABundle)
    );
    assert_eq!((handle.pushed(), s.queued_bytes()), (0, 0));

    // A bundle split anywhere after its first byte goes out bare.
    let data = bpv7_like(20);
    push_in_chunks(&mut s, &mut handle, &data, 1);
    s.finish(handle).unwrap();
    assert_eq!(drain(&mut s), vec![data]);
}

#[test]
fn begin_refuses_what_enqueue_refuses() {
    let mut s = sender(64, 4);
    assert_eq!(s.begin(0, SendOptions::default()), Err(Error::Empty));
    let handles: Vec<_> = (0..4).map(|_| begin(&mut s, 200)).collect();
    assert_eq!(
        s.begin(200, SendOptions::default()),
        Err(Error::Window(TransferError::WindowFull {
            window_size: window_size(4)
        }))
    );
    // A fitting bundle takes no window slot.
    let fitting = begin(&mut s, 10);
    assert!(s.cancel(fitting));
    for handle in handles {
        assert!(s.cancel(handle));
    }
}

#[test]
fn the_high_watermark_counts_pushed_bytes() {
    let mut s = sender_with_high_watermark(64, 50, LinkFraming::FixedSize);
    let data = patterned(200);
    let mut handle = begin(&mut s, data.len());
    // Beginning takes no bytes; pushing does, and is never refused for it.
    assert!(!s.is_send_queue_high());
    s.push(&mut handle, data.slice(..100)).unwrap();
    assert!(s.is_send_queue_high());
    s.push(&mut handle, data.slice(100..)).unwrap();
    s.finish(handle).unwrap();
    drain(&mut s);
    assert!(!s.is_send_queue_high());
}

#[test]
fn a_push_is_taken_whole_however_far_it_passes_the_high_watermark() {
    let mut s = sender_with_high_watermark(64, 10, LinkFraming::FixedSize);
    let data = patterned(200);
    let mut handle = begin(&mut s, data.len());
    s.push(&mut handle, data.slice(..9)).unwrap();
    assert!(!s.is_send_queue_high());
    s.push(&mut handle, data.slice(9..10)).unwrap();
    assert!(s.is_send_queue_high());
    // At the watermark a push is still taken, and taken whole.
    s.push(&mut handle, data.slice(10..)).unwrap();
    assert_eq!(s.queued_bytes(), 200);
    s.finish(handle).unwrap();
    assert_eq!(transfer_data(&drain(&mut s), 0), data);
    assert_eq!(s.queued_bytes(), 0);
}

#[test]
fn a_transfer_waiting_on_its_producer_is_passed_over() {
    const PDU: usize = 64;
    let data = patterned(200);
    let mut s = sender(PDU, 16);
    let mut waiting = begin(&mut s, data.len());
    // Segment 0 goes out, and the rest waits on bytes not yet pushed.
    s.push(&mut waiting, data.slice(..PDU)).unwrap();
    let mut waiting_pdus = vec![s.next_pdu().unwrap().data];
    assert_eq!(s.next_pdu(), None);

    let small = bundle(10);
    let small_id = enqueue(&mut s, small).unwrap();
    let other = patterned(100);
    let other_id = enqueue(&mut s, other.clone()).unwrap();

    // Both go ahead of the waiting transfer, the segmented one filling the
    // tail behind the Bundle Message.
    let first = s.next_pdu().unwrap();
    assert_eq!(
        &first.carried[..],
        &[carried(small_id, true), carried(other_id, false)]
    );
    let mut other_pdus = vec![first.data];
    while let Some(pdu) = s.next_pdu() {
        assert!(pdu.carried.iter().all(|c| c.id == other_id));
        other_pdus.push(pdu.data);
    }
    // The second transfer begun, after the waiting one.
    assert_eq!(transfer_data(&other_pdus, 1), other);
    assert!(s.has_pending());

    s.push(&mut waiting, data.slice(PDU..)).unwrap();
    s.finish(waiting).unwrap();
    waiting_pdus.extend(drain(&mut s));
    // The sender's first transfer number.
    assert_eq!(transfer_data(&waiting_pdus, 0), data);
    assert!(!s.has_pending());
}

#[test]
fn a_transfer_passed_over_fills_the_tail_once_the_room_shrinks_to_its_bytes() {
    const PDU: usize = 64;
    let data = patterned(200);
    let capacity = first_capacity(PDU, data.len());
    let framing = PDU - capacity;
    // Half a segment pushed: too few for a full PDU's segment 0, which
    // waits for all of its bytes, but enough to fill a tail that holds
    // exactly them.
    let pushed = capacity.div_ceil(2);
    let mut s = sender(PDU, 16);
    let mut handle = begin(&mut s, data.len());
    s.push(&mut handle, data.slice(..pushed)).unwrap();
    // A Bundle Message queued behind it that leaves that tail.
    let small = bundle(PDU - HEADER_SIZE - framing - pushed);
    let small_id = enqueue(&mut s, small.clone()).unwrap();

    let pdu = s.next_pdu().unwrap();
    assert_eq!(
        &pdu.carried[..],
        &[carried(small_id, true), carried(handle.id(), false)]
    );
    assert_eq!(
        decode_all(pdu.data),
        [
            bundle_with(vec![], small),
            segment_with(
                0,
                0,
                vec![HintItem::BundleLength(data.len() as u64)],
                data.slice(..pushed)
            ),
        ]
    );
}

#[test]
fn every_outstanding_transfer_can_end_in_one_pdu_within_the_list_bound() {
    const PDU: usize = 64;
    const WINDOW: u16 = 4;
    // Two full segments one byte short of the bundle, so each End carries
    // one byte, 13 bytes as a message.
    let hints_len = encoded_hints_len(&[HintItem::BundleLength(2 * PDU as u64)]);
    let len =
        (PDU - HEADER_SIZE - SEGMENT_FIELDS - hints_len) + (PDU - HEADER_SIZE - SEGMENT_FIELDS) + 1;
    assert_eq!(
        first_capacity(PDU, len),
        PDU - HEADER_SIZE - SEGMENT_FIELDS - hints_len
    );
    let mut s = sender(PDU, WINDOW);
    let mut handles: Vec<_> = (0..WINDOW).map(|_| begin(&mut s, len)).collect();
    for handle in &mut handles {
        s.push(handle, patterned(len - 1)).unwrap();
    }
    assert_eq!(drain(&mut s).len(), 2 * usize::from(WINDOW));

    // Four Ends and a Bundle Message fill the PDU exactly.
    let fitting = enqueue(&mut s, bundle(PDU - 4 * 13 - HEADER_SIZE)).unwrap();
    let mut expected = Vec::new();
    for mut handle in handles {
        s.push(&mut handle, Bytes::from_static(&[0])).unwrap();
        expected.push(carried(s.finish(handle).unwrap(), true));
    }
    expected.push(carried(fitting, true));

    let mut list = CarriedList::with_capacity(PDU / 32 + usize::from(WINDOW));
    let capacity = list.capacity();
    let pdu = s.next_pdu_into(&mut list).unwrap();
    assert_eq!(&list[..], &expected[..]);
    assert_eq!(list.capacity(), capacity);
    // Nothing but the five messages, so no padding.
    assert_eq!(decode_all(pdu).len(), 5);
    assert!(!s.has_pending());
}

fn half_cut_sender(pdu_size: usize) -> Sender {
    sender_with(pdu_size, |c| {
        c.segment_cut_strategy = SegmentCutStrategy::Half
    })
}

#[test]
fn a_half_cut_sends_half_a_segment_without_waiting_for_the_rest() {
    const PDU: usize = 64;
    let capacity = PDU - HEADER_SIZE - SEGMENT_FIELDS;
    let data = patterned(200);
    let first = first_capacity(PDU, data.len());
    // A chunk of segment 0 and exactly half of segment 1.
    let chunk = first + capacity.div_ceil(2);

    let mut full = sender(PDU, 16);
    let mut handle = begin(&mut full, data.len());
    full.push(&mut handle, data.slice(..chunk)).unwrap();
    assert_eq!(drain(&mut full).len(), 1);

    let mut half = half_cut_sender(PDU);
    let mut handle = begin(&mut half, data.len());
    // The sender's first transfer number.
    let t = 0;
    half.push(&mut handle, data.slice(..chunk)).unwrap();
    let mut pdus = drain(&mut half);
    assert_eq!(pdus.len(), 2);
    assert_eq!(transfer_data(&pdus[1..], t), data[first..chunk]);
    assert_eq!(half.queued_bytes(), 0);

    // One byte short of half a segment waits.
    let short = chunk + capacity.div_ceil(2) - 1;
    half.push(&mut handle, data.slice(chunk..short)).unwrap();
    assert_eq!(half.next_pdu(), None);

    half.push(&mut handle, data.slice(short..)).unwrap();
    half.finish(handle).unwrap();
    pdus.extend(drain(&mut half));
    assert_eq!(transfer_data(&pdus, t), data);
}

#[cfg(target_pointer_width = "64")]
#[test]
fn begin_refuses_a_bundle_whose_last_index_could_reach_u32_max() {
    const PDU: usize = 64;
    // Segments cut short carry at least half a segment, so the last index
    // is bounded by one more than the bundle over half a segment.
    let half = (PDU - HEADER_SIZE - SEGMENT_FIELDS).div_ceil(2);
    let refused = half * (u32::MAX as usize - 1);
    let mut s = sender(PDU, 16);
    assert_eq!(
        s.begin(refused, SendOptions::default()),
        Err(Error::TooManySegments {
            len: refused,
            pdu_size: PDU
        })
    );
    assert_eq!(
        s.begin(refused - 1, SendOptions::default())
            .map(|h| h.id().kind()),
        Ok(SendKind::Transfer)
    );
}

#[test]
fn a_producer_gated_on_push_readiness_never_waits_on_itself() {
    const PDU: usize = 64;
    // A watermark under one segment: gating each push on the watermark would
    // stop at the first push, with no segment complete to drain.
    const WATERMARK: usize = 10;
    const CHUNK: usize = 7;
    let data = patterned(500);
    let mut s = sender_with_high_watermark(PDU, WATERMARK, LinkFraming::FixedSize);
    let mut handle = begin(&mut s, data.len());
    // The sender's first transfer number.
    let t = 0;

    let mut pdus = Vec::new();
    for start in (0..data.len()).step_by(CHUNK) {
        while !s.is_push_ready(&handle) {
            pdus.push(s.next_pdu().expect("a segment is complete").data);
        }
        let end = (start + CHUNK).min(data.len());
        s.push(&mut handle, data.slice(start..end)).unwrap();
        assert!(s.queued_bytes() < (WATERMARK + PDU + CHUNK) as u64);
    }
    s.finish(handle).unwrap();
    pdus.extend(drain(&mut s));
    assert_eq!(transfer_data(&pdus, t), data);
}

#[test]
fn push_readiness_past_the_high_watermark_follows_whether_the_bundle_waits() {
    const PDU: usize = 64;
    let data = patterned(200);
    let mut s = sender_with_high_watermark(PDU, 50, LinkFraming::FixedSize);
    let mut handle = begin(&mut s, data.len());
    s.push(&mut handle, data.slice(..30)).unwrap();
    assert!(s.is_push_ready(&handle));

    // Past the watermark, still short of segment 0: the bundle waits on its
    // producer, so its push is admitted.
    let mut other = begin(&mut s, data.len());
    s.push(&mut other, data.slice(..30)).unwrap();
    assert!(s.is_send_queue_high());
    assert!(s.is_push_ready(&handle));

    // Once segment 0 is complete, it can go out, so the producer drains.
    s.push(&mut handle, data.slice(30..60)).unwrap();
    assert!(!s.is_push_ready(&handle));
    assert!(s.is_push_ready(&other));
    s.next_pdu().unwrap();
    assert!(!s.is_send_queue_high());
    assert!(s.is_push_ready(&handle));
}

#[test]
fn push_readiness_admits_a_fitting_bundle_and_one_not_in_progress() {
    let mut s = sender_with_high_watermark(64, 4, LinkFraming::FixedSize);
    let mut fitting = begin(&mut s, 10);
    s.push(&mut fitting, Bytes::from_static(b"01234")).unwrap();
    assert!(s.is_send_queue_high());
    // A fitting bundle goes out only once whole.
    assert!(s.is_push_ready(&fitting));

    let mut cancelled = begin(&mut s, 200);
    assert!(s.cancel(cancelled.id()));
    assert!(s.is_push_ready(&cancelled));
    assert_eq!(
        s.push(&mut cancelled, Bytes::from_static(b"x")),
        Err(Error::NotInProgress)
    );
}

const FLUSH: NextPduOptions = NextPduOptions { flush: true };

#[test]
fn a_flush_sends_what_a_waiting_transfer_has_buffered() {
    const PDU: usize = 64;
    let data = patterned(200);
    let mut s = sender(PDU, 16);
    let mut fitting = begin(&mut s, 10);
    s.push(&mut fitting, Bytes::from_static(b"0123")).unwrap();
    // A bundle that fits one PDU is not queued until it is whole.
    assert_eq!(s.next_pdu_with(FLUSH), None);

    let mut handle = begin(&mut s, data.len());
    // The sender's first transfer number: the fitting bundle takes none.
    let t = 0;
    s.push(&mut handle, data.slice(..10)).unwrap();
    assert_eq!(s.next_pdu(), None);

    let pdu = s.next_pdu_with(FLUSH).unwrap();
    assert_eq!(&pdu.carried[..], &[carried(handle.id(), false)]);
    let Message::TransferSegment(m) = &decode_all(pdu.data.clone())[0] else {
        panic!("expected a segment");
    };
    assert_eq!((m.segment_index, &m.data[..]), (0, &data[..10]));
    assert_eq!(s.queued_bytes(), 4);
    // Nothing left buffered to flush.
    assert_eq!(s.next_pdu_with(FLUSH), None);

    let mut pdus = vec![pdu.data];
    s.push(&mut handle, data.slice(10..)).unwrap();
    s.finish(handle).unwrap();
    pdus.extend(drain(&mut s));
    assert_eq!(transfer_data(&pdus, t), data);
}

#[test]
fn a_flush_fills_the_room_left_with_waiting_transfers_in_queue_order() {
    const PDU: usize = 100;
    let data = patterned(200);
    let first_framing = PDU - first_capacity(PDU, data.len());
    let mut s = sender(PDU, 16);
    let mut a = begin(&mut s, data.len());
    let mut b = begin(&mut s, data.len());
    s.push(&mut a, data.slice(..10)).unwrap();
    s.push(&mut b, data.slice(..40)).unwrap();
    let fitting = enqueue(&mut s, bundle(20)).unwrap();

    // The ready Bundle Message first, then each waiting transfer's bytes,
    // b's cut to the room left.
    let b_fits = PDU - (HEADER_SIZE + 20) - (first_framing + 10) - first_framing;
    assert!(b_fits < 40);
    let pdu = s.next_pdu_with(FLUSH).unwrap();
    assert_eq!(
        &pdu.carried[..],
        &[
            carried(fitting, true),
            carried(a.id(), false),
            carried(b.id(), false)
        ]
    );
    let messages = decode_all(pdu.data.clone());
    assert_eq!(messages.len(), 3, "the PDU is full, so it has no padding");
    let mut pdus = vec![pdu.data];
    // a and b are the sender's first two transfer numbers.
    assert_eq!(transfer_data(&pdus, 0), data[..10]);
    assert_eq!(transfer_data(&pdus, 1), data[..b_fits]);

    // What b could not fit goes in the next flush, as segment 1.
    let pdu = s.next_pdu_with(FLUSH).unwrap();
    assert_eq!(&pdu.carried[..], &[carried(b.id(), false)]);
    let Message::TransferSegment(m) = &decode_all(pdu.data.clone())[0] else {
        panic!("expected a segment");
    };
    assert_eq!((m.segment_index, &m.data[..]), (1, &data[b_fits..40]));
    pdus.push(pdu.data);

    for (mut handle, from) in [(a, 10), (b, 40)] {
        s.push(&mut handle, data.slice(from..)).unwrap();
        s.finish(handle).unwrap();
    }
    pdus.extend(drain(&mut s));
    for t in [0, 1] {
        assert_eq!(transfer_data(&pdus, t), data);
    }
}

#[test]
fn a_flush_does_not_reorder_past_a_message_that_does_not_fit() {
    const PDU: usize = 64;
    let data = patterned(200);
    let mut s = sender(PDU, 16);
    let mut ahead = begin(&mut s, data.len());
    s.push(&mut ahead, data.slice(..10)).unwrap();
    let small = enqueue(&mut s, bundle(10)).unwrap();
    let large = enqueue(&mut s, bundle(PDU - HEADER_SIZE)).unwrap();
    let mut behind = begin(&mut s, data.len());
    s.push(&mut behind, data.slice(..10)).unwrap();

    let pdu = s.next_pdu_with(FLUSH).unwrap();
    assert_eq!(
        &pdu.carried[..],
        &[carried(small, true), carried(ahead.id(), false)]
    );
    let pdu = s.next_pdu_with(FLUSH).unwrap();
    assert_eq!(&pdu.carried[..], &[carried(large, true)]);
    let pdu = s.next_pdu_with(FLUSH).unwrap();
    assert_eq!(&pdu.carried[..], &[carried(behind.id(), false)]);
}
