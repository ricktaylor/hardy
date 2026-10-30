//! Integration tests for the sender's wake-ups without the `tower` feature:
//! `poll_admit` and `poll_push_ready` for producers, and
//! `poll_next_pdu_write` and `poll_next_pdu_vectored` for the drain.

mod common;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::sync_channel,
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, Thread},
    time::Duration,
};

use bytes::Bytes;
use hardy_btpu::sender::{
    CarriedList, Error, LinkFraming, PduSlices, SendHandle, SendId, SendOptions, Sender,
};

use self::common::{bundle, drain, enqueue, sender, sender_with_high_watermark};

// A waker that counts its wakes.  `std::task::Wake` on an `Arc` supplies
// the vtable, so no unsafe is needed.
struct Count(AtomicUsize);

impl Count {
    fn waker() -> (Arc<Self>, Waker) {
        let count = Arc::new(Self(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        (count, waker)
    }

    fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// A waker that unparks an OS thread, as a drain on a thread of its own
// would pass.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

// A sender whose window of 4 is filled by segmented bundles, and their ids,
// oldest first.
fn saturated_sender() -> (Sender, Vec<SendId>) {
    let mut sender = sender(32, 4);
    let ids = (0..4)
        .map(|_| enqueue(&mut sender, bundle(200)).unwrap())
        .collect();
    (sender, ids)
}

#[test]
fn a_sender_with_room_admits_at_once() {
    let mut sender = sender(32, 4);
    let (count, waker) = Count::waker();
    assert_eq!(
        sender.poll_admit(&mut Context::from_waker(&waker)),
        Poll::Ready(())
    );
    assert_eq!(count.get(), 0);
}

#[test]
fn a_producer_parked_on_a_full_window_is_woken_when_the_oldest_end_is_packed() {
    let (mut sender, _) = saturated_sender();
    let (count, waker) = Count::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(sender.poll_admit(&mut cx), Poll::Pending);

    // No PDU before the one carrying the oldest transfer's End opens the
    // window, so none of them wakes the producer.
    let mut carried = CarriedList::default();
    let mut out = vec![0; 32];
    while !sender.is_window_available() {
        assert_eq!(count.get(), 0);
        assert!(
            sender
                .next_pdu_write(&mut carried, &mut out)
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(count.get(), 1);
    assert_eq!(sender.poll_admit(&mut cx), Poll::Ready(()));
}

// The window bounds the span from the oldest outstanding transfer number,
// so releasing a newer one opens nothing.
#[test]
fn cancelling_a_newer_transfer_does_not_wake_a_producer_parked_on_the_window() {
    let (mut sender, ids) = saturated_sender();
    let (count, waker) = Count::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(sender.poll_admit(&mut cx), Poll::Pending);

    assert!(sender.cancel(ids[3]));
    assert_eq!(count.get(), 0);
    assert_eq!(sender.poll_admit(&mut cx), Poll::Pending);

    assert!(sender.cancel(ids[0]));
    assert_eq!(count.get(), 1);
    assert_eq!(sender.poll_admit(&mut cx), Poll::Ready(()));
}

#[test]
fn a_producer_parked_on_the_high_watermark_is_woken_when_the_queue_drains() {
    let mut sender = sender_with_high_watermark(256, 4, LinkFraming::FixedSize);
    enqueue(&mut sender, Bytes::from_static(b"tiny")).unwrap();
    let (count, waker) = Count::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(sender.poll_admit(&mut cx), Poll::Pending);

    assert_eq!(drain(&mut sender).len(), 1);
    assert_eq!(count.get(), 1);
    assert_eq!(sender.poll_admit(&mut cx), Poll::Ready(()));
}

#[test]
fn every_parked_producer_is_woken_and_each_once() {
    let (mut sender, ids) = saturated_sender();
    let (first, first_waker) = Count::waker();
    let (second, second_waker) = Count::waker();
    let mut first_cx = Context::from_waker(&first_waker);
    let mut second_cx = Context::from_waker(&second_waker);
    // The first producer polls twice and is registered once.
    assert_eq!(sender.poll_admit(&mut first_cx), Poll::Pending);
    assert_eq!(sender.poll_admit(&mut first_cx), Poll::Pending);
    assert_eq!(sender.poll_admit(&mut second_cx), Poll::Pending);

    assert!(sender.cancel(ids[0]));
    assert_eq!((first.get(), second.get()), (1, 1));
}

#[test]
fn a_drain_parked_on_an_empty_queue_is_woken_by_an_enqueue() {
    let mut sender = sender(64, 4);
    let (count, waker) = Count::waker();
    let mut cx = Context::from_waker(&waker);
    let mut carried = CarriedList::default();
    let mut out = vec![0; 64];
    assert!(
        sender
            .poll_next_pdu_write(&mut cx, &mut carried, &mut out)
            .is_pending()
    );

    let id = enqueue(&mut sender, bundle(20)).unwrap();
    assert_eq!(count.get(), 1);
    assert_eq!(
        sender
            .poll_next_pdu_write(&mut cx, &mut carried, &mut out)
            .map(Result::unwrap),
        Poll::Ready(64)
    );
    assert_eq!(carried.completed().collect::<Vec<_>>(), [id]);
}

#[test]
fn a_drain_with_a_short_buffer_is_refused_rather_than_parked() {
    let mut sender = sender(64, 4);
    let (count, waker) = Count::waker();
    let mut out = vec![0; 63];
    let polled = sender.poll_next_pdu_write(
        &mut Context::from_waker(&waker),
        &mut CarriedList::default(),
        &mut out,
    );
    assert!(matches!(
        polled,
        Poll::Ready(Err(Error::BufferTooShort {
            needed: 64,
            len: 63
        }))
    ));

    enqueue(&mut sender, bundle(20)).unwrap();
    assert_eq!(count.get(), 0);
}

#[test]
fn a_vectored_drain_is_woken_by_the_push_that_readies_a_segment_and_not_before() {
    let mut sender = sender(64, 4);
    let mut handle = sender.begin(200, SendOptions::default()).unwrap();
    let data = bundle(200);
    let (count, waker) = Count::waker();
    let mut cx = Context::from_waker(&waker);
    let mut carried = CarriedList::default();
    let mut out = PduSlices::default();
    assert_eq!(
        sender.poll_next_pdu_vectored(&mut cx, &mut carried, &mut out),
        Poll::Pending
    );

    // Too few bytes for segment 0, so the drain would find nothing.
    sender.push(&mut handle, data.slice(..10)).unwrap();
    assert_eq!(count.get(), 0);

    sender.push(&mut handle, data.slice(10..)).unwrap();
    assert_eq!(count.get(), 1);
    assert_eq!(
        sender.poll_next_pdu_vectored(&mut cx, &mut carried, &mut out),
        Poll::Ready(64)
    );
    sender.finish(handle).unwrap();
}

#[test]
fn a_drain_polled_from_another_task_replaces_the_first() {
    let mut sender = sender(64, 4);
    let (first, first_waker) = Count::waker();
    let (second, second_waker) = Count::waker();
    let mut carried = CarriedList::default();
    let mut out = vec![0; 64];
    for waker in [&first_waker, &second_waker] {
        assert!(
            sender
                .poll_next_pdu_write(&mut Context::from_waker(waker), &mut carried, &mut out)
                .is_pending()
        );
    }

    enqueue(&mut sender, bundle(20)).unwrap();
    assert_eq!((first.get(), second.get()), (0, 1));
}

// The shape of a CLA whose drain is an OS thread filling a shared-memory
// ring: the thread parks while nothing is ready, with a waker that unparks
// it, and a producer on another thread shares the sender through a mutex.
#[test]
fn a_drain_on_an_os_thread_parks_until_a_producer_enqueues() {
    let shared = Arc::new(Mutex::new(sender(64, 4)));
    let (parking, parked) = sync_channel(1);
    let (done, drained) = sync_channel(1);
    thread::spawn({
        let shared = shared.clone();
        move || {
            let waker = Waker::from(Arc::new(Unpark(thread::current())));
            let mut cx = Context::from_waker(&waker);
            let mut carried = CarriedList::default();
            let mut out = vec![0; 64];
            loop {
                let polled =
                    shared
                        .lock()
                        .unwrap()
                        .poll_next_pdu_write(&mut cx, &mut carried, &mut out);
                match polled {
                    Poll::Ready(len) => {
                        let completed = carried.completed().collect::<Vec<_>>();
                        done.send((len.unwrap(), completed)).unwrap();
                        return;
                    }
                    // A wake between the unlock and the park leaves the
                    // thread's token set, so the park returns at once; a
                    // spurious return polls again.
                    Poll::Pending => {
                        let _ = parking.try_send(());
                        thread::park();
                    }
                }
            }
        }
    });

    parked.recv().unwrap();
    let id = enqueue(&mut shared.lock().unwrap(), bundle(20)).unwrap();
    // The timeout only bounds a regression: a drain that is never woken
    // stays parked.
    assert_eq!(
        drained.recv_timeout(Duration::from_secs(60)).unwrap(),
        (64, vec![id])
    );
}

// Two transfers begun on a 64-byte PDU with a 100-byte watermark, with
// `first` and `second` bytes pushed, the queue high and neither push ready.
fn two_waiting_pushes(first: usize, second: usize) -> (Sender, SendHandle, SendHandle) {
    let mut sender = sender_with_high_watermark(64, 100, LinkFraming::FixedSize);
    let data = bundle(1000);
    let mut a = sender.begin(1000, SendOptions::default()).unwrap();
    let mut b = sender.begin(1000, SendOptions::default()).unwrap();
    sender.push(&mut a, data.slice(..first)).unwrap();
    sender.push(&mut b, data.slice(..second)).unwrap();
    assert!(sender.is_send_queue_high());
    assert!(!sender.is_push_ready(&a) && !sender.is_push_ready(&b));
    (sender, a, b)
}

// Pack one 64-byte PDU, which there must be.
fn pack_one(sender: &mut Sender) {
    let mut out = vec![0; 64];
    assert!(
        sender
            .next_pdu_write(&mut CarriedList::default(), &mut out)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_push_below_the_watermark_or_for_a_fitting_bundle_is_ready_at_once() {
    let mut sender = sender_with_high_watermark(64, 100, LinkFraming::FixedSize);
    let (count, waker) = Count::waker();
    let mut cx = Context::from_waker(&waker);
    let segmented = sender.begin(1000, SendOptions::default()).unwrap();
    assert_eq!(sender.poll_push_ready(&segmented, &mut cx), Poll::Ready(()));

    enqueue(&mut sender, bundle(200)).unwrap();
    assert!(sender.is_send_queue_high());
    let fitting = sender.begin(20, SendOptions::default()).unwrap();
    assert_eq!(sender.poll_push_ready(&fitting, &mut cx), Poll::Ready(()));
    assert_eq!(count.get(), 0);
}

#[test]
fn a_parked_push_is_woken_when_its_buffered_segments_are_packed_and_not_before() {
    let (mut sender, a, b) = two_waiting_pushes(150, 400);
    let (a_count, a_waker) = Count::waker();
    let (b_count, b_waker) = Count::waker();
    let mut a_cx = Context::from_waker(&a_waker);
    assert_eq!(sender.poll_push_ready(&a, &mut a_cx), Poll::Pending);
    assert_eq!(
        sender.poll_push_ready(&b, &mut Context::from_waker(&b_waker)),
        Poll::Pending
    );

    // The PDU that leaves `a` waiting on its producer wakes it, and no
    // earlier one; `b` still has segments to send and the queue is high.
    while !sender.is_push_ready(&a) {
        assert_eq!(a_count.get(), 0);
        pack_one(&mut sender);
    }
    assert_eq!((a_count.get(), b_count.get()), (1, 0));
    assert!(sender.is_send_queue_high());
    assert_eq!(sender.poll_push_ready(&a, &mut a_cx), Poll::Ready(()));
}

#[test]
fn every_parked_push_is_woken_when_the_queue_drops_below_the_watermark() {
    let (mut sender, a, b) = two_waiting_pushes(80, 80);
    let big = enqueue(&mut sender, bundle(500)).unwrap();
    let (a_count, a_waker) = Count::waker();
    let (b_count, b_waker) = Count::waker();
    assert_eq!(
        sender.poll_push_ready(&a, &mut Context::from_waker(&a_waker)),
        Poll::Pending
    );
    assert_eq!(
        sender.poll_push_ready(&b, &mut Context::from_waker(&b_waker)),
        Poll::Pending
    );

    // 160 bytes stay queued, still at the watermark.
    assert!(sender.cancel(big));
    assert_eq!((a_count.get(), b_count.get()), (0, 0));
    assert!(sender.cancel(b.id()));
    assert!(!sender.is_send_queue_high());
    assert_eq!((a_count.get(), b_count.get()), (1, 1));
}

#[test]
fn cancelling_a_bundle_wakes_the_push_parked_on_it() {
    let (mut sender, a, _b) = two_waiting_pushes(150, 400);
    let (count, waker) = Count::waker();
    let mut cx = Context::from_waker(&waker);
    assert_eq!(sender.poll_push_ready(&a, &mut cx), Poll::Pending);

    assert!(sender.cancel(a.id()));
    assert!(sender.is_send_queue_high());
    assert_eq!(count.get(), 1);
    assert_eq!(sender.poll_push_ready(&a, &mut cx), Poll::Ready(()));
}

#[test]
fn a_push_parked_after_the_last_byte_is_woken_when_the_end_is_packed() {
    let mut sender = sender_with_high_watermark(64, 100, LinkFraming::FixedSize);
    let data = bundle(1000);
    let mut a = sender.begin(150, SendOptions::default()).unwrap();
    let mut b = sender.begin(1000, SendOptions::default()).unwrap();
    sender.push(&mut a, data.slice(..150)).unwrap();
    sender.push(&mut b, data.slice(..400)).unwrap();
    let (count, waker) = Count::waker();
    assert_eq!(
        sender.poll_push_ready(&a, &mut Context::from_waker(&waker)),
        Poll::Pending
    );

    // Every byte is pushed, so the bundle never waits on its producer;
    // only the release of its number makes the push ready.
    while sender.is_outstanding(a.id()) {
        assert_eq!(count.get(), 0);
        pack_one(&mut sender);
    }
    assert!(sender.is_send_queue_high());
    assert_eq!(count.get(), 1);
}

#[test]
fn a_push_polled_from_another_task_replaces_the_first() {
    let (mut sender, a, _b) = two_waiting_pushes(150, 400);
    let (first, first_waker) = Count::waker();
    let (second, second_waker) = Count::waker();
    for waker in [&first_waker, &second_waker] {
        assert_eq!(
            sender.poll_push_ready(&a, &mut Context::from_waker(waker)),
            Poll::Pending
        );
    }

    assert!(sender.cancel(a.id()));
    assert_eq!((first.get(), second.get()), (0, 1));
}
