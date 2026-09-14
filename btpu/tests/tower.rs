//! Integration tests for the `tower` feature.

#![cfg(feature = "tower")]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use bytes::{Bytes, BytesMut};
use futures::task::noop_waker_ref;
use futures_core::Stream;
use hardy_btpu::{
    codec::hint::HintItem,
    receiver::{MaxBundleSize, Receiver, ReceiverEvent},
    sender::{Enqueued, PduSize, SendOpts, SendQueueDepth, SendRequest, Sender},
    transfer::WindowSize,
};
use tower::{Service, ServiceBuilder, ServiceExt};

fn poll_stream_until_idle(sender: &mut Sender) -> Vec<BytesMut> {
    let mut pdus = Vec::new();
    let mut cx = Context::from_waker(noop_waker_ref());
    loop {
        match std::pin::Pin::new(&mut *sender).poll_next(&mut cx) {
            Poll::Ready(Some(pdu)) => pdus.push(pdu),
            // The sender is documented as a perpetual source; treating None
            // as "idle" here would silently mask that regression.
            Poll::Ready(None) => panic!("sender stream must never finish"),
            Poll::Pending => break,
        }
    }
    pdus
}

#[tokio::test]
async fn receiver_service_round_trip() {
    let mut receiver: Receiver = Receiver::new(WindowSize::default(), MaxBundleSize::default());

    // Build a Bundle-message PDU by going through Sender (so we don't have to
    // hand-craft wire bytes).
    let mut sender = Sender::new(
        PduSize::try_from(256).unwrap(),
        WindowSize::default(),
        SendQueueDepth::default(),
        0,
    );
    Service::call(&mut sender, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    let pdu = poll_stream_until_idle(&mut sender).pop().unwrap();

    let events = Service::call(&mut receiver, pdu.freeze()).await.unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ReceiverEvent::BundleReceived { data: b, .. } => assert_eq!(b.as_ref(), b"hello"),
        other => panic!("expected BundleReceived, got {other:?}"),
    }
}

#[tokio::test]
async fn sender_service_enqueue_then_stream_drain() {
    let pdu_size = 64;
    let mut sender = Sender::new(
        PduSize::try_from(pdu_size).unwrap(),
        WindowSize::default(),
        SendQueueDepth::default(),
        0,
    );
    let mut receiver = Receiver::new(WindowSize::default(), MaxBundleSize::default());

    let original = Bytes::from(vec![0x42; 200]);

    let transfer = Service::call(&mut sender, original.clone()).await.unwrap();
    assert!(
        matches!(transfer, Enqueued::Transfer(_)),
        "200-byte bundle in 64-byte PDU must segment"
    );

    let pdus = poll_stream_until_idle(&mut sender);
    assert!(!pdus.is_empty());

    let mut received = None;
    for pdu in pdus {
        for event in Service::call(&mut receiver, pdu.freeze()).await.unwrap() {
            if let ReceiverEvent::BundleReceived { data: b, .. } = event {
                received = Some(b);
            }
        }
    }
    assert_eq!(received.unwrap().as_ref(), original.as_ref());
}

#[tokio::test]
async fn sender_service_with_layer() {
    // Compile-time + runtime check that Sender slots into ServiceBuilder.
    let sender = Sender::new(
        PduSize::default(),
        WindowSize::default(),
        SendQueueDepth::default(),
        0,
    );
    let mut svc = ServiceBuilder::new().concurrency_limit(4).service(sender);

    let res = ServiceExt::<Bytes>::ready(&mut svc)
        .await
        .unwrap()
        .call(Bytes::from_static(b"small"))
        .await
        .unwrap();
    // Small bundle in default 1500-byte PDU goes as a single Bundle message,
    // so no transfer number is allocated.
    assert_eq!(res, Enqueued::Bundle);
}

#[tokio::test]
async fn sender_service_poll_ready_blocks_when_window_full() {
    let mut sender = Sender::new(
        PduSize::try_from(32).unwrap(),
        WindowSize::try_from(4).unwrap(),
        SendQueueDepth::default(),
        0,
    );

    // Fill the window with 4 segmented bundles (each big enough to force
    // segmentation, so each consumes a transfer-number slot).
    let big = Bytes::from(vec![0u8; 200]);
    for _ in 0..4 {
        Service::call(&mut sender, big.clone()).await.unwrap();
    }

    // Window saturated: poll_ready must be Pending.
    let mut cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        <Sender as Service<Bytes>>::poll_ready(&mut sender, &mut cx),
        Poll::Pending
    ));

    // Completing the newest transfer frees a count slot but not window span
    // (transfer 0 still anchors the window), so poll_ready stays Pending.
    sender.complete(3);
    assert!(matches!(
        <Sender as Service<Bytes>>::poll_ready(&mut sender, &mut cx),
        Poll::Pending
    ));

    // Completing the oldest advances the window — poll_ready becomes Ready.
    sender.complete(0);
    assert!(matches!(
        <Sender as Service<Bytes>>::poll_ready(&mut sender, &mut cx),
        Poll::Ready(Ok(()))
    ));
}

#[tokio::test]
async fn sender_stream_pending_when_idle() {
    let mut sender = Sender::new(
        PduSize::default(),
        WindowSize::default(),
        SendQueueDepth::default(),
        0,
    );
    let mut cx = Context::from_waker(noop_waker_ref());

    // No pending: poll_next must be Pending (not Ready(None) — the sender
    // is a perpetual source until dropped).
    assert!(matches!(
        std::pin::Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Pending
    ));

    // After enqueue, poll_next yields Some(pdu).
    Service::call(&mut sender, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    assert!(matches!(
        std::pin::Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Ready(Some(_))
    ));

    // Drained: back to Pending.
    assert!(matches!(
        std::pin::Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Pending
    ));
}

/// A tiny waker that raises a flag when woken.  `std::task::Wake` on an
/// `Arc` supplies the vtable, so no unsafe is needed.
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

#[tokio::test]
async fn sender_complete_wakes_pending_enqueue_task() {
    let mut sender = Sender::new(
        PduSize::try_from(32).unwrap(),
        WindowSize::try_from(4).unwrap(),
        SendQueueDepth::default(),
        0,
    );
    let big = Bytes::from(vec![0u8; 200]);
    for _ in 0..4 {
        Service::call(&mut sender, big.clone()).await.unwrap();
    }

    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        <Sender as Service<Bytes>>::poll_ready(&mut sender, &mut cx),
        Poll::Pending
    ));
    assert!(!woke.raised());

    // complete() should wake our stored waker.
    sender.complete(0);
    assert!(woke.raised(), "complete() should wake the enqueue waker");
}

#[tokio::test]
async fn sender_service_poll_ready_blocks_when_send_queue_full() {
    // Small bundles take the unsegmented path and never allocate a window
    // slot, so only the send-queue depth can bound them.
    let mut sender = Sender::new(
        PduSize::try_from(256).unwrap(),
        WindowSize::default(),
        SendQueueDepth::try_from(2).unwrap(),
        0,
    );
    for _ in 0..2 {
        Service::call(&mut sender, Bytes::from_static(b"tiny"))
            .await
            .unwrap();
    }

    // The window is untouched, yet the sender must exert backpressure: the
    // queue is at its configured depth.
    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        <Sender as Service<Bytes>>::poll_ready(&mut sender, &mut cx),
        Poll::Pending
    ));
    assert!(!woke.raised());

    // Draining a PDU frees queue capacity and wakes the parked task.
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        std::pin::Pin::new(&mut sender).poll_next(&mut noop_cx),
        Poll::Ready(Some(_))
    ));
    assert!(
        woke.raised(),
        "draining next_pdu should wake the enqueue waker"
    );
    assert!(matches!(
        <Sender as Service<Bytes>>::poll_ready(&mut sender, &mut noop_cx),
        Poll::Ready(Ok(()))
    ));
}

#[tokio::test]
async fn sender_enqueue_wakes_pending_drain_task() {
    let mut sender = Sender::new(
        PduSize::default(),
        WindowSize::default(),
        SendQueueDepth::default(),
        0,
    );

    // Nothing pending: the stream parks and registers our waker.
    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        std::pin::Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Pending
    ));
    assert!(!woke.raised());

    // Enqueueing a bundle pushes a message and must wake the drain task.
    Service::call(&mut sender, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    assert!(woke.raised(), "enqueue should wake the drain waker");
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        std::pin::Pin::new(&mut sender).poll_next(&mut noop_cx),
        Poll::Ready(Some(_))
    ));
}

#[tokio::test]
async fn sender_cancel_wakes_pending_drain_task() {
    let mut sender = Sender::new(
        PduSize::try_from(32).unwrap(),
        WindowSize::try_from(4).unwrap(),
        SendQueueDepth::default(),
        0,
    );

    // A segmented transfer, fully drained: the stream parks again.
    let Enqueued::Transfer(transfer) = Service::call(&mut sender, Bytes::from(vec![0u8; 200]))
        .await
        .unwrap()
    else {
        panic!("expected a segmented transfer")
    };
    poll_stream_until_idle(&mut sender);

    let woke = Flag::new();
    let waker = Waker::from(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        std::pin::Pin::new(&mut sender).poll_next(&mut cx),
        Poll::Pending
    ));
    assert!(!woke.raised());

    // cancel() queues a Transfer Cancel message and must wake the drain task.
    sender.cancel(transfer);
    assert!(woke.raised(), "cancel should wake the drain waker");
    let mut noop_cx = Context::from_waker(noop_waker_ref());
    assert!(matches!(
        std::pin::Pin::new(&mut sender).poll_next(&mut noop_cx),
        Poll::Ready(Some(_))
    ));
}

#[tokio::test]
async fn sender_service_send_request_carries_hints() {
    let mut sender = Sender::new(
        PduSize::try_from(64).unwrap(),
        WindowSize::default(),
        SendQueueDepth::default(),
        0,
    );
    let mut receiver = Receiver::new(WindowSize::default(), MaxBundleSize::default());

    let correlator = HintItem::Unknown {
        hint_type: 0x41,
        value: Bytes::from_static(b"\x2A"),
    };
    let request = SendRequest {
        data: Bytes::from(vec![0x42; 200]),
        opts: SendOpts {
            hints: vec![correlator.clone()],
        },
    };
    Service::call(&mut sender, request).await.unwrap();

    let mut received = None;
    for pdu in poll_stream_until_idle(&mut sender) {
        for event in Service::call(&mut receiver, pdu.freeze()).await.unwrap() {
            if let ReceiverEvent::BundleReceived { data, hints } = event {
                received = Some((data, hints));
            }
        }
    }
    let (data, hints) = received.unwrap();
    assert_eq!(data.len(), 200);
    assert!(
        hints.contains(&correlator),
        "the caller hint must survive to the completion event"
    );
}
