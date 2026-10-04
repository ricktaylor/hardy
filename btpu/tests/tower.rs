//! Integration tests for the `tower` feature.

#![cfg(feature = "tower")]

mod common;

use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use bytes::Bytes;
use futures::{executor::block_on, task::noop_waker_ref};
use futures_core::Stream;
use hardy_btpu::{
    codec::hint::HintItem,
    receiver::{Receiver, ReceiverConfig, ReceiverEvent},
    sender::{
        BundleFraming, Error, LinkFraming, Pdu, SendId, SendKind, SendOptions, SendRequest, Sender,
        SenderConfig,
    },
};
use tower::{Service, ServiceBuilder, ServiceExt};

use self::common::{
    bpv7_like, bundle, cancel, carried, decode_all, received, received_with, receiver, sender,
    sender_with_high_watermark, unknown_hint,
};

fn poll_stream_until_idle(sender: &mut Sender) -> Vec<Bytes> {
    let mut pdus = Vec::new();
    let mut cx = Context::from_waker(noop_waker_ref());
    loop {
        match poll_next(sender, &mut cx) {
            Poll::Ready(Some(pdu)) => pdus.push(pdu.data),
            // The sender is documented as a perpetual source; treating None
            // as "idle" here would silently mask that regression.
            Poll::Ready(None) => panic!("sender stream must never finish"),
            Poll::Pending => break,
        }
    }
    pdus
}

fn poll_next(sender: &mut Sender, cx: &mut Context<'_>) -> Poll<Option<Pdu>> {
    Pin::new(sender).poll_next(cx)
}

fn poll_ready(sender: &mut Sender, cx: &mut Context<'_>) -> Poll<()> {
    Service::poll_ready(sender, cx).map(|r| r.unwrap())
}

fn call(sender: &mut Sender, data: Bytes) -> SendId {
    block_on(Service::call(sender, SendRequest::from(data))).unwrap()
}

fn receive_all(receiver: &mut Receiver, pdus: Vec<Bytes>) -> Vec<ReceiverEvent> {
    pdus.into_iter()
        .flat_map(|pdu| block_on(Service::call(&mut *receiver, pdu)).unwrap())
        .collect()
}

// A sender whose window of 4 is filled by segmented bundles, and the id of
// the oldest.
fn saturated_sender() -> (Sender, SendId) {
    let mut sender = sender(32, 4);
    let oldest = call(&mut sender, bundle(200));
    for _ in 1..4 {
        call(&mut sender, bundle(200));
    }
    (sender, oldest)
}

// A tiny waker that raises a flag when woken.  `std::task::Wake` on an
// `Arc` supplies the vtable, so no unsafe is needed.
struct Flag(AtomicBool);

impl Flag {
    fn waker() -> (Arc<Self>, Waker) {
        let flag = Arc::new(Self(AtomicBool::new(false)));
        let waker = Waker::from(flag.clone());
        (flag, waker)
    }

    fn raised(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn receiver_service_round_trip() {
    let mut receiver = Receiver::new(ReceiverConfig::default());
    let mut sender = sender(256, 16);
    call(&mut sender, Bytes::from_static(b"hello"));
    let pdu = poll_stream_until_idle(&mut sender).pop().unwrap();

    let events = block_on(Service::call(&mut receiver, pdu)).unwrap();
    assert_eq!(events, vec![received(b"hello")]);
}

#[test]
fn sender_service_enqueue_then_stream_drain() {
    let mut sender = sender(64, 16);
    let mut receiver = Receiver::new(ReceiverConfig::default());
    let original = bundle(200);

    assert_eq!(
        call(&mut sender, original.clone()).kind(),
        SendKind::Transfer,
        "200-byte bundle in 64-byte PDU must segment"
    );

    let pdus = poll_stream_until_idle(&mut sender);
    assert_eq!(
        receive_all(&mut receiver, pdus),
        vec![received_with(original, vec![HintItem::BundleLength(200)])]
    );
}

#[test]
fn sender_service_with_layer() {
    // Compile-time + runtime check that Sender slots into ServiceBuilder.
    let sender = Sender::new(SenderConfig::default(), 0);
    let mut svc = ServiceBuilder::new().concurrency_limit(4).service(sender);

    let res = block_on(async {
        svc.ready()
            .await
            .unwrap()
            .call(Bytes::from_static(b"small").into())
            .await
    })
    .unwrap();
    // Small bundle in default 1500-byte PDU goes as a single Bundle message,
    // so no transfer number is allocated.
    assert_eq!(res.kind(), SendKind::Message);
}

#[test]
fn sender_service_poll_ready_blocks_until_the_oldest_end_drains() {
    let (mut sender, oldest) = saturated_sender();
    let mut cx = Context::from_waker(noop_waker_ref());
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);

    // Draining PDU by PDU: poll_ready turns Ready exactly when the oldest
    // transfer's End has been packed, freeing its slot.
    loop {
        assert!(matches!(
            poll_next(&mut sender, &mut cx),
            Poll::Ready(Some(_))
        ));
        let ready = poll_ready(&mut sender, &mut cx).is_ready();
        assert_eq!(ready, !sender.is_outstanding(oldest));
        if ready {
            break;
        }
    }
}

#[test]
fn sender_stream_pending_when_idle() {
    let mut sender = Sender::new(SenderConfig::default(), 0);
    let mut cx = Context::from_waker(noop_waker_ref());

    // No pending: poll_next must be Pending (not Ready(None); the sender
    // is a perpetual source until dropped).
    assert_eq!(poll_next(&mut sender, &mut cx), Poll::Pending);

    call(&mut sender, Bytes::from_static(b"hello"));
    assert!(matches!(
        poll_next(&mut sender, &mut cx),
        Poll::Ready(Some(_))
    ));

    // Drained: back to Pending.
    assert_eq!(poll_next(&mut sender, &mut cx), Poll::Pending);
}

#[test]
fn sender_stream_yields_the_bundles_each_pdu_carries() {
    let mut sender = sender(64, 4);
    let id = call(&mut sender, bundle(100));
    let mut cx = Context::from_waker(noop_waker_ref());
    let mut pdus: Vec<Pdu> = Vec::new();
    while let Poll::Ready(Some(pdu)) = poll_next(&mut sender, &mut cx) {
        pdus.push(pdu);
    }

    // 100 bytes in 64-byte PDUs spans two: the head, then the End.
    let bundles: Vec<_> = pdus.into_iter().map(|p| p.carried.to_vec()).collect();
    assert_eq!(
        bundles,
        vec![vec![carried(id, false)], vec![carried(id, true)]]
    );
}

#[test]
fn sender_drain_wakes_pending_enqueue_task_when_window_full() {
    let (mut sender, oldest) = saturated_sender();

    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    // Draining the End of the oldest transfer frees a window slot, so it
    // wakes the parked task, which then finds the service ready.  No
    // earlier PDU frees anything, so none of them wakes it.
    drain_until_window_opens(&mut sender, oldest, &[&woke]);
    assert!(woke.raised(), "draining should wake the enqueue waker");
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Ready(()));
}

// Drain `sender` one PDU at a time until the PDU carrying the End of
// `oldest`, asserting that no flag is raised before it.
fn drain_until_window_opens(sender: &mut Sender, oldest: SendId, flags: &[&Flag]) {
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    loop {
        for flag in flags {
            assert!(!flag.raised(), "woken before capacity freed");
        }
        let Poll::Ready(Some(pdu)) = poll_next(sender, &mut noop_cx) else {
            panic!("the queue ran dry before the oldest transfer ended");
        };
        if pdu.carried.contains(&carried(oldest, true)) {
            return;
        }
    }
}

#[test]
fn sender_cancel_wakes_pending_enqueue_task() {
    let (mut sender, oldest) = saturated_sender();

    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    // Cancelling the oldest transfer frees the window and must wake the
    // parked task, which then finds the service ready.
    assert!(sender.cancel(oldest));
    assert!(woke.raised(), "cancel() should wake the enqueue waker");
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Ready(()));
}

#[test]
fn every_parked_producer_is_woken() {
    // Two producers sharing a Sender each park in poll_ready; when
    // capacity frees both must wake, or the one not woken sleeps forever
    // once the other has nothing more to send.
    let (mut sender, oldest) = saturated_sender();

    let (a, waker_a) = Flag::waker();
    let (b, waker_b) = Flag::waker();
    assert_eq!(
        poll_ready(&mut sender, &mut Context::from_waker(&waker_a)),
        Poll::Pending
    );
    assert_eq!(
        poll_ready(&mut sender, &mut Context::from_waker(&waker_b)),
        Poll::Pending
    );
    // Re-polling with a waker already registered does not displace the
    // other one.
    assert_eq!(
        poll_ready(&mut sender, &mut Context::from_waker(&waker_a)),
        Poll::Pending
    );

    drain_until_window_opens(&mut sender, oldest, &[&a, &b]);
    assert!(a.raised(), "producer A was not woken");
    assert!(b.raised(), "producer B was not woken");
}

// The documented fan-in pattern: one lock acquisition spans `poll_ready`
// and `call`, and the guard is dropped before parking, so the admission a
// producer was told about cannot be taken by another producer in between
// and a `Pending` producer never holds the drain out.
fn admit(shared: &Mutex<Sender>, cx: &mut Context<'_>, data: Bytes) -> Poll<SendId> {
    let mut sender = shared.lock().unwrap();
    match poll_ready(&mut sender, cx) {
        Poll::Ready(()) => Poll::Ready(call(&mut sender, data)),
        Poll::Pending => Poll::Pending,
    }
}

#[test]
fn producers_admitted_under_one_lock_hold_never_see_window_full() {
    // Window of 4, all four slots taken.  Two producers compete for the
    // next slot; each is either admitted or parked, never told Ready and
    // then refused with WindowFull by `call`.
    let shared = Mutex::new(sender(32, 4));
    let big = Bytes::from(vec![0u8; 200]);
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    for _ in 0..4 {
        assert_eq!(
            admit(&shared, &mut noop_cx, big.clone()).map(SendId::kind),
            Poll::Ready(SendKind::Transfer)
        );
    }

    let (a, waker_a) = Flag::waker();
    let (b, waker_b) = Flag::waker();
    assert_eq!(
        admit(&shared, &mut Context::from_waker(&waker_a), big.clone()),
        Poll::Pending
    );
    assert_eq!(
        admit(&shared, &mut Context::from_waker(&waker_b), big.clone()),
        Poll::Pending
    );

    // Drain until exactly one slot frees (the first transfer's End is
    // packed).  The drain task takes the lock per PDU, which the parked
    // producers are not holding.
    while !shared.lock().unwrap().is_window_available() {
        let mut sender = shared.lock().unwrap();
        assert!(matches!(
            poll_next(&mut sender, &mut noop_cx),
            Poll::Ready(Some(_))
        ));
    }
    assert!(a.raised() && b.raised());

    // Both race for the one slot: the first is admitted, the second parks
    // again rather than failing.
    assert_eq!(
        admit(&shared, &mut Context::from_waker(&waker_a), big.clone()).map(SendId::kind),
        Poll::Ready(SendKind::Transfer)
    );
    assert_eq!(
        admit(&shared, &mut Context::from_waker(&waker_b), big.clone()),
        Poll::Pending
    );
    while !shared.lock().unwrap().is_window_available() {
        let mut sender = shared.lock().unwrap();
        assert!(matches!(
            poll_next(&mut sender, &mut noop_cx),
            Poll::Ready(Some(_))
        ));
    }
    assert_eq!(
        admit(&shared, &mut Context::from_waker(&waker_b), big).map(SendId::kind),
        Poll::Ready(SendKind::Transfer)
    );
}

#[test]
fn cancelling_a_queued_bundle_wakes_a_producer_parked_on_the_high_watermark() {
    let mut sender = sender_with_high_watermark(256, 4, LinkFraming::FixedSize);
    let id = call(&mut sender, Bytes::from_static(b"tiny"));

    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);

    assert!(sender.cancel(id));
    assert!(woke.raised(), "cancel() should wake the enqueue waker");
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Ready(()));
}

#[test]
fn draining_a_bare_frame_wakes_a_producer_parked_on_the_high_watermark() {
    let mut sender =
        sender_with_high_watermark(256, 20, LinkFraming::variable(BundleFraming::Bare));
    assert_eq!(call(&mut sender, bpv7_like(20)).kind(), SendKind::Bare);

    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);

    assert_eq!(poll_stream_until_idle(&mut sender), vec![bpv7_like(20)]);
    assert!(
        woke.raised(),
        "popping a bare frame should wake the enqueue waker"
    );
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Ready(()));
}

#[test]
fn stream_stays_pending_after_cancel_empties_the_queue() {
    let mut sender = sender(256, 16);
    let id = call(&mut sender, Bytes::from_static(b"tiny"));
    assert!(sender.cancel(id));

    let mut cx = Context::from_waker(noop_waker_ref());
    assert_eq!(poll_next(&mut sender, &mut cx), Poll::Pending);
}

#[test]
fn a_ready_service_takes_a_request_larger_than_the_high_watermark() {
    // `poll_ready` cannot see the request, so it admits on the queue alone,
    // and `call` takes the bundle whole.
    let mut sender = sender_with_high_watermark(256, 8, LinkFraming::FixedSize);
    let mut cx = Context::from_waker(noop_waker_ref());
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Ready(()));
    call(&mut sender, bundle(100));
    assert_eq!(sender.queued_bytes(), 100);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);
}

#[test]
fn sender_service_poll_ready_blocks_when_send_queue_high() {
    // Small bundles take the unsegmented path and never allocate a window
    // slot, so only the send queue's high watermark holds them back.
    let mut sender = sender_with_high_watermark(256, 8, LinkFraming::FixedSize);
    for _ in 0..2 {
        call(&mut sender, Bytes::from_static(b"tiny"));
    }

    // The window is untouched, yet the sender must exert backpressure: the
    // queue is at its high watermark.
    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    // Draining a PDU frees queue capacity and wakes the parked task.
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        poll_next(&mut sender, &mut noop_cx),
        Poll::Ready(Some(_))
    ));
    assert!(
        woke.raised(),
        "draining next_pdu should wake the enqueue waker"
    );
    assert_eq!(poll_ready(&mut sender, &mut noop_cx), Poll::Ready(()));
}

#[test]
fn a_push_that_readies_a_segment_wakes_the_pending_drain_task() {
    let mut sender = sender(64, 16);
    let mut handle = sender.begin(200, SendOptions::default()).unwrap();

    // Begun but with nothing pushed, the transfer supplies nothing: the
    // stream parks.
    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_next(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    sender.push(&mut handle, bundle(200)).unwrap();
    assert!(woke.raised(), "push should wake the drain waker");
    sender.finish(handle).unwrap();
    assert!(!poll_stream_until_idle(&mut sender).is_empty());
}

#[test]
fn a_push_that_leaves_the_transfer_waiting_does_not_wake_the_drain() {
    let mut sender = sender(64, 16);
    let mut handle = sender.begin(200, SendOptions::default()).unwrap();
    let data = bundle(200);

    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_next(&mut sender, &mut cx), Poll::Pending);

    // Too few bytes for segment 0, so the drain would find nothing.
    sender.push(&mut handle, data.slice(..10)).unwrap();
    assert!(!woke.raised());

    sender.push(&mut handle, data.slice(10..)).unwrap();
    assert!(
        woke.raised(),
        "the push that readies segment 0 wakes the drain"
    );
    sender.finish(handle).unwrap();
}

#[test]
fn cancelling_a_started_transfer_wakes_the_drain_parked_on_it() {
    let mut sender = sender(32, 16);
    let mut handle = sender.begin(200, SendOptions::default()).unwrap();
    sender.push(&mut handle, bundle(200).slice(..17)).unwrap();

    // Segment 0 goes out, then the transfer waits on its producer and the
    // drain parks.
    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        poll_next(&mut sender, &mut cx),
        Poll::Ready(Some(_))
    ));
    assert_eq!(poll_next(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    assert!(sender.cancel(handle));
    assert!(woke.raised(), "the queued Transfer Cancel wakes the drain");
    let Poll::Ready(Some(pdu)) = poll_next(&mut sender, &mut cx) else {
        panic!("the Transfer Cancel is ready");
    };
    assert_eq!(decode_all(pdu.data)[0], cancel(0));
}

#[test]
fn an_underrun_finish_wakes_the_drain_parked_on_its_transfer() {
    let mut sender = sender(32, 16);
    let mut handle = sender.begin(200, SendOptions::default()).unwrap();
    sender.push(&mut handle, bundle(200).slice(..17)).unwrap();

    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        poll_next(&mut sender, &mut cx),
        Poll::Ready(Some(_))
    ));
    assert_eq!(poll_next(&mut sender, &mut cx), Poll::Pending);

    assert_eq!(
        sender.finish(handle),
        Err(Error::Underrun {
            total_len: 200,
            pushed: 17
        })
    );
    assert!(woke.raised(), "the queued Transfer Cancel wakes the drain");
    let Poll::Ready(Some(pdu)) = poll_next(&mut sender, &mut cx) else {
        panic!("the Transfer Cancel is ready");
    };
    assert_eq!(decode_all(pdu.data)[0], cancel(0));
}

#[test]
fn sender_enqueue_wakes_pending_drain_task() {
    let mut sender = Sender::new(SenderConfig::default(), 0);

    // Nothing pending: the stream parks and registers our waker.
    let (woke, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_next(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    call(&mut sender, Bytes::from_static(b"hello"));
    assert!(woke.raised(), "enqueue should wake the drain waker");
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        poll_next(&mut sender, &mut noop_cx),
        Poll::Ready(Some(_))
    ));
}

#[test]
fn sender_service_send_request_carries_hints() {
    let mut sender = sender(64, 16);
    let mut receiver = receiver(16, usize::MAX);

    let correlator = unknown_hint(0x41, b"\x2A");
    let request = SendRequest {
        data: bundle(200),
        options: SendOptions {
            hints: vec![correlator.clone()].into(),
        },
    };
    block_on(Service::call(&mut sender, request)).unwrap();

    let pdus = poll_stream_until_idle(&mut sender);
    assert_eq!(
        receive_all(&mut receiver, pdus),
        vec![received_with(
            bundle(200),
            vec![HintItem::BundleLength(200), correlator]
        )]
    );
}
