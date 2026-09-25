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
        BundleFraming, BundleTransferId, Carried, LinkFraming, Pdu, SendOptions, SendQueueDepth,
        SendRequest, Sender, SenderConfig,
    },
};
use tower::{Service, ServiceBuilder, ServiceExt};

use self::common::{bpv7_like, receiver, sender, sender_config};

fn poll_stream_until_idle(sender: &mut Sender) -> Vec<Bytes> {
    let mut pdus = Vec::new();
    let mut cx = Context::from_waker(noop_waker_ref());
    loop {
        match Pin::new(&mut *sender).poll_next(&mut cx) {
            Poll::Ready(Some(pdu)) => pdus.push(pdu.data),
            // The sender is documented as a perpetual source; treating None
            // as "idle" here would silently mask that regression.
            Poll::Ready(None) => panic!("sender stream must never finish"),
            Poll::Pending => break,
        }
    }
    pdus
}

fn poll_ready(sender: &mut Sender, cx: &mut Context<'_>) -> Poll<()> {
    Service::poll_ready(sender, cx).map(|r| r.unwrap())
}

fn call(sender: &mut Sender, data: Bytes) -> BundleTransferId {
    block_on(Service::call(sender, SendRequest::from(data))).unwrap()
}

// A tiny waker that raises a flag when woken.  `std::task::Wake` on an
// `Arc` supplies the vtable, so no unsafe is needed.
struct Flag(AtomicBool);

impl Flag {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(false)))
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
    assert_eq!(
        events,
        vec![ReceiverEvent::BundleReceived {
            data: Bytes::from_static(b"hello"),
            hints: vec![],
        }]
    );
}

#[test]
fn sender_service_enqueue_then_stream_drain() {
    let mut sender = sender(64, 16);
    let mut receiver = Receiver::new(ReceiverConfig::default());
    let original = Bytes::from(vec![0x42; 200]);

    assert!(
        matches!(
            call(&mut sender, original.clone()),
            BundleTransferId::Transfer(_)
        ),
        "200-byte bundle in 64-byte PDU must segment"
    );

    let pdus = poll_stream_until_idle(&mut sender);
    assert!(!pdus.is_empty());

    let mut received = None;
    for pdu in pdus {
        for event in block_on(Service::call(&mut receiver, pdu)).unwrap() {
            if let ReceiverEvent::BundleReceived { data, .. } = event {
                received = Some(data);
            }
        }
    }
    assert_eq!(received, Some(original));
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
    assert_eq!(res, BundleTransferId::Message(0));
}

#[test]
fn sender_service_poll_ready_blocks_until_the_oldest_end_drains() {
    let mut sender = sender(32, 4);
    // Fill the window with 4 segmented bundles.
    let big = Bytes::from(vec![0u8; 200]);
    for _ in 0..4 {
        call(&mut sender, big.clone());
    }

    // Window saturated: poll_ready must be Pending.
    let mut cx = Context::from_waker(noop_waker_ref());
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);

    // Draining PDU by PDU: poll_ready turns Ready exactly when transfer
    // 0's End has been packed, freeing its slot.
    loop {
        assert!(matches!(
            Pin::new(&mut sender).poll_next(&mut cx),
            Poll::Ready(Some(_))
        ));
        let ready = poll_ready(&mut sender, &mut cx).is_ready();
        assert_eq!(ready, !sender.is_transfer_outstanding(0));
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
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Pending
    ));

    call(&mut sender, Bytes::from_static(b"hello"));
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Ready(Some(_))
    ));

    // Drained: back to Pending.
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Pending
    ));
}

#[test]
fn sender_stream_yields_the_bundles_each_pdu_carries() {
    let mut sender = sender(64, 4);
    let id = call(&mut sender, Bytes::from(vec![0x42; 100]));
    let mut cx = Context::from_waker(noop_waker_ref());
    let mut pdus: Vec<Pdu> = Vec::new();
    while let Poll::Ready(Some(pdu)) = Pin::new(&mut sender).poll_next(&mut cx) {
        pdus.push(pdu);
    }

    // 100 bytes in 64-byte PDUs spans two: the head, then the End.
    let bundles: Vec<_> = pdus.into_iter().map(|p| p.bundles).collect();
    assert_eq!(
        bundles,
        vec![
            vec![Carried {
                id,
                completes: false
            }],
            vec![Carried {
                id,
                completes: true
            }],
        ]
    );
}

#[test]
fn sender_drain_wakes_pending_enqueue_task_when_window_full() {
    let mut sender = sender(32, 4);
    let big = Bytes::from(vec![0u8; 200]);
    for _ in 0..4 {
        call(&mut sender, big.clone());
    }

    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    // Draining a PDU is what frees capacity, so it must wake the parked
    // task; the task then re-polls and parks again if the window is still
    // full.
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut noop_cx),
        Poll::Ready(Some(_))
    ));
    assert!(woke.raised(), "draining should wake the enqueue waker");
}

#[test]
fn sender_cancel_wakes_pending_enqueue_task() {
    let mut sender = sender(32, 4);
    let big = Bytes::from(vec![0u8; 200]);
    for _ in 0..4 {
        call(&mut sender, big.clone());
    }

    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    // Cancelling the oldest transfer frees the window and must wake the
    // parked task, which then finds the service ready.
    assert!(sender.cancel(BundleTransferId::Transfer(0)));
    assert!(woke.raised(), "cancel() should wake the enqueue waker");
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Ready(()));
}

#[test]
fn every_parked_producer_is_woken() {
    // Two producers sharing a Sender through a mutex each park in
    // poll_ready; when capacity frees both must wake, or the one not
    // woken sleeps forever once the other has nothing more to send.
    let mut sender = sender(32, 4);
    let big = Bytes::from(vec![0u8; 200]);
    for _ in 0..4 {
        call(&mut sender, big.clone());
    }

    let a = Flag::new();
    let b = Flag::new();
    let waker_a = Waker::from(a.clone());
    let waker_b = Waker::from(b.clone());
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

    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut noop_cx),
        Poll::Ready(Some(_))
    ));
    assert!(a.raised(), "producer A was not woken");
    assert!(b.raised(), "producer B was not woken");
}

// The documented fan-in pattern: one lock acquisition spans `poll_ready`
// and `call`, and the guard is dropped before parking, so the admission a
// producer was told about cannot be taken by another producer in between
// and a `Pending` producer never holds the drain out.
fn admit(shared: &Mutex<Sender>, cx: &mut Context<'_>, data: Bytes) -> Poll<BundleTransferId> {
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
        assert!(matches!(
            admit(&shared, &mut noop_cx, big.clone()),
            Poll::Ready(BundleTransferId::Transfer(_))
        ));
    }

    let a = Flag::new();
    let b = Flag::new();
    let waker_a = Waker::from(a.clone());
    let waker_b = Waker::from(b.clone());
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
            Pin::new(&mut *sender).poll_next(&mut noop_cx),
            Poll::Ready(Some(_))
        ));
    }
    assert!(a.raised() && b.raised());

    // Both race for the one slot: the first is admitted, the second parks
    // again rather than failing.
    assert!(matches!(
        admit(&shared, &mut Context::from_waker(&waker_a), big.clone()),
        Poll::Ready(BundleTransferId::Transfer(_))
    ));
    assert_eq!(
        admit(&shared, &mut Context::from_waker(&waker_b), big.clone()),
        Poll::Pending
    );
    while !shared.lock().unwrap().is_window_available() {
        let mut sender = shared.lock().unwrap();
        assert!(matches!(
            Pin::new(&mut *sender).poll_next(&mut noop_cx),
            Poll::Ready(Some(_))
        ));
    }
    assert!(matches!(
        admit(&shared, &mut Context::from_waker(&waker_b), big),
        Poll::Ready(BundleTransferId::Transfer(_))
    ));
}

#[test]
fn cancelling_a_queued_bundle_wakes_a_producer_parked_on_queue_depth() {
    let mut sender = Sender::new(
        SenderConfig {
            send_queue_depth: SendQueueDepth::try_from(1).unwrap(),
            ..sender_config(256, 16)
        },
        0,
    );
    let id = call(&mut sender, Bytes::from_static(b"tiny"));

    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);

    assert!(sender.cancel(id));
    assert!(woke.raised(), "cancel() should wake the enqueue waker");
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Ready(()));
}

#[test]
fn draining_a_bare_frame_wakes_a_producer_parked_on_queue_depth() {
    let mut sender = Sender::new(
        SenderConfig {
            send_queue_depth: SendQueueDepth::try_from(1).unwrap(),
            link_framing: LinkFraming::Variable {
                bundle_framing: BundleFraming::Bare,
            },
            ..sender_config(256, 16)
        },
        0,
    );
    assert_eq!(call(&mut sender, bpv7_like(20)), BundleTransferId::Bare(0));

    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
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
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Pending
    ));
}

#[test]
fn sender_service_poll_ready_blocks_when_send_queue_full() {
    // Small bundles take the unsegmented path and never allocate a window
    // slot, so only the send-queue depth can bound them.
    let mut sender = Sender::new(
        SenderConfig {
            send_queue_depth: SendQueueDepth::try_from(2).unwrap(),
            ..sender_config(256, 16)
        },
        0,
    );
    for _ in 0..2 {
        call(&mut sender, Bytes::from_static(b"tiny"));
    }

    // The window is untouched, yet the sender must exert backpressure: the
    // queue is at its configured depth.
    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert_eq!(poll_ready(&mut sender, &mut cx), Poll::Pending);
    assert!(!woke.raised());

    // Draining a PDU frees queue capacity and wakes the parked task.
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut noop_cx),
        Poll::Ready(Some(_))
    ));
    assert!(
        woke.raised(),
        "draining next_pdu should wake the enqueue waker"
    );
    assert_eq!(poll_ready(&mut sender, &mut noop_cx), Poll::Ready(()));
}

#[test]
fn sender_enqueue_wakes_pending_drain_task() {
    let mut sender = Sender::new(SenderConfig::default(), 0);

    // Nothing pending: the stream parks and registers our waker.
    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Pending
    ));
    assert!(!woke.raised());

    // Enqueueing a bundle pushes a message and must wake the drain task.
    call(&mut sender, Bytes::from_static(b"hello"));
    assert!(woke.raised(), "enqueue should wake the drain waker");
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        Pin::new(&mut sender).poll_next(&mut noop_cx),
        Poll::Ready(Some(_))
    ));
}

#[test]
fn sender_service_send_request_carries_hints() {
    let mut sender = sender(64, 16);
    let mut receiver = receiver(16, usize::MAX);

    let correlator = HintItem::Unknown {
        hint_type: 0x41,
        value: Bytes::from_static(b"\x2A"),
    };
    let request = SendRequest {
        data: Bytes::from(vec![0x42; 200]),
        options: SendOptions {
            hints: vec![correlator.clone()],
        },
    };
    block_on(Service::call(&mut sender, request)).unwrap();

    let mut received = None;
    for pdu in poll_stream_until_idle(&mut sender) {
        for event in block_on(Service::call(&mut receiver, pdu)).unwrap() {
            if let ReceiverEvent::BundleReceived { data, hints } = event {
                received = Some((data, hints));
            }
        }
    }
    let (data, hints) = received.unwrap();
    assert_eq!(data.len(), 200);
    assert_eq!(hints, vec![HintItem::BundleLength(200), correlator]);
}
