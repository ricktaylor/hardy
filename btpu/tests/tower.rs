//! Integration tests for the `tower` feature.

#![cfg(feature = "tower")]

use bytes::{Bytes, BytesMut};
use futures::task::noop_waker_ref;
use futures_core::Stream;
use hardy_btpu::receiver::{Receiver, ReceiverConfig, ReceiverEvent};
use hardy_btpu::sender::{Sender, SenderConfig};
use std::task::{Context, Poll};
use tower::{Service, ServiceBuilder, ServiceExt};

fn poll_stream_until_idle(sender: &mut Sender) -> Vec<BytesMut> {
    let mut pdus = Vec::new();
    let mut cx = Context::from_waker(noop_waker_ref());
    loop {
        match std::pin::Pin::new(&mut *sender).poll_next(&mut cx) {
            Poll::Ready(Some(pdu)) => pdus.push(pdu),
            Poll::Ready(None) => break,
            Poll::Pending => break,
        }
    }
    pdus
}

#[tokio::test]
async fn receiver_service_round_trip() {
    let mut receiver: Receiver = Receiver::new(ReceiverConfig {
        window_size: 16,
        max_bundle_size: None,
    });

    // Build a Bundle-message PDU by going through Sender (so we don't have to
    // hand-craft wire bytes).
    let mut sender = Sender::new(
        SenderConfig {
            pdu_size: 256,
            window_size: 16,
        },
        0,
    );
    Service::call(&mut sender, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    let pdu = poll_stream_until_idle(&mut sender).pop().unwrap();

    let events = Service::call(&mut receiver, pdu.freeze()).await.unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ReceiverEvent::BundleReceived(b) => assert_eq!(b.as_ref(), b"hello"),
        other => panic!("expected BundleReceived, got {other:?}"),
    }
}

#[tokio::test]
async fn sender_service_enqueue_then_stream_drain() {
    let pdu_size = 64;
    let mut sender = Sender::new(
        SenderConfig {
            pdu_size,
            window_size: 16,
        },
        0,
    );
    let mut receiver = Receiver::new(ReceiverConfig {
        window_size: 16,
        max_bundle_size: None,
    });

    let original = Bytes::from(vec![0x42; 200]);

    let transfer = Service::call(&mut sender, original.clone()).await.unwrap();
    assert!(
        transfer.is_some(),
        "200-byte bundle in 64-byte PDU must segment"
    );

    let pdus = poll_stream_until_idle(&mut sender);
    assert!(!pdus.is_empty());

    let mut received = None;
    for pdu in pdus {
        for event in Service::call(&mut receiver, pdu.freeze()).await.unwrap() {
            if let ReceiverEvent::BundleReceived(b) = event {
                received = Some(b);
            }
        }
    }
    assert_eq!(received.unwrap().as_ref(), original.as_ref());
}

#[tokio::test]
async fn sender_service_with_layer() {
    // Compile-time + runtime check that Sender slots into ServiceBuilder.
    let sender = Sender::new(SenderConfig::default(), 0);
    let mut svc = ServiceBuilder::new().concurrency_limit(4).service(sender);

    let res = svc
        .ready()
        .await
        .unwrap()
        .call(Bytes::from_static(b"small"))
        .await
        .unwrap();
    // Small bundle in default 1500-byte PDU goes as a single Bundle message,
    // so no transfer number is allocated.
    assert_eq!(res, None);
}

#[tokio::test]
async fn sender_service_poll_ready_blocks_when_window_full() {
    let mut sender = Sender::new(
        SenderConfig {
            pdu_size: 32,
            window_size: 4,
        },
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
        Service::poll_ready(&mut sender, &mut cx),
        Poll::Pending
    ));

    // Free a slot — poll_ready becomes Ready again.
    sender.complete(0);
    assert!(matches!(
        Service::poll_ready(&mut sender, &mut cx),
        Poll::Ready(Ok(()))
    ));
}

#[tokio::test]
async fn sender_stream_pending_when_idle() {
    let mut sender = Sender::new(SenderConfig::default(), 0);
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

#[tokio::test]
async fn sender_complete_wakes_pending_enqueue_task() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{RawWaker, RawWakerVTable, Waker};

    // A tiny waker that flips an AtomicBool when woken.
    fn flag_waker(flag: Arc<AtomicBool>) -> Waker {
        unsafe fn clone(p: *const ()) -> RawWaker {
            unsafe {
                let arc = Arc::from_raw(p as *const AtomicBool);
                let cloned = arc.clone();
                core::mem::forget(arc);
                RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
            }
        }
        unsafe fn wake(p: *const ()) {
            unsafe {
                let arc = Arc::from_raw(p as *const AtomicBool);
                arc.store(true, Ordering::SeqCst);
            }
        }
        unsafe fn wake_by_ref(p: *const ()) {
            unsafe {
                let arc = Arc::from_raw(p as *const AtomicBool);
                arc.store(true, Ordering::SeqCst);
                core::mem::forget(arc);
            }
        }
        unsafe fn drop_(p: *const ()) {
            unsafe {
                drop(Arc::from_raw(p as *const AtomicBool));
            }
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_);
        let ptr = Arc::into_raw(flag) as *const ();
        unsafe { Waker::from_raw(RawWaker::new(ptr, &VTABLE)) }
    }

    let mut sender = Sender::new(
        SenderConfig {
            pdu_size: 32,
            window_size: 4,
        },
        0,
    );
    let big = Bytes::from(vec![0u8; 200]);
    for _ in 0..4 {
        Service::call(&mut sender, big.clone()).await.unwrap();
    }

    let woke = Arc::new(AtomicBool::new(false));
    let waker = flag_waker(woke.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        Service::poll_ready(&mut sender, &mut cx),
        Poll::Pending
    ));
    assert!(!woke.load(Ordering::SeqCst));

    // complete() should wake our stored waker.
    sender.complete(0);
    assert!(
        woke.load(Ordering::SeqCst),
        "complete() should wake the enqueue waker"
    );
}
