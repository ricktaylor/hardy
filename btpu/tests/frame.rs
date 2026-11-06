//! BTP-U over a frame service (the `frame` feature): a sender and a
//! receiver bound to the halves of an in-memory service, the sender's
//! framing taken from the service's size, and each PDU packed into the
//! frame memory the service lends, once the sender holds it.

#![cfg(feature = "frame")]

mod common;

use std::{
    collections::VecDeque,
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use bytes::Bytes;
use futures::{Stream, StreamExt, task::noop_waker_ref};
use hardy_btpu::{
    OutOfRange,
    codec::hint::HintItem,
    frame,
    receiver::ReceiverEvent,
    sender::{
        BundleFraming, Carried, CarriedList, LinkFraming, PduSize, SendId, SendOptions,
        SenderConfig,
    },
};
use hardy_frame::{FrameRx, FrameSize, FrameTx, Permit, SendError};

use common::{
    Event, bundle, carried, patterned, reassembled_with, received_with, receiver, sender_config,
};

const WINDOW: u16 = 4;

// A frame service held in memory: one slot of frame memory a permit lends,
// and the frames sent through it, in order, which its receive half yields.
struct Link {
    size: FrameSize,
    slot: Mutex<Vec<u8>>,
    frames: Mutex<VecDeque<Bytes>>,
    refusing: AtomicBool,
}

impl Link {
    fn new(size: FrameSize) -> Self {
        Self {
            size,
            slot: Mutex::new(vec![0; size.max()]),
            frames: Mutex::new(VecDeque::new()),
            refusing: AtomicBool::new(false),
        }
    }

    fn frames(&self) -> Vec<Bytes> {
        self.frames.lock().unwrap().iter().cloned().collect()
    }

    // Takes the slot, as another user of the half holding a permit would.
    fn hold(&self) -> MutexGuard<'_, Vec<u8>> {
        self.slot.lock().unwrap()
    }

    // Whether every frame sent from now on is refused as dropped.
    fn refuse(&self, refusing: bool) {
        self.refusing.store(refusing, Ordering::Relaxed);
    }

    // The receive half: the frames sent so far, then the end of the stream.
    fn rx(&self) -> Rx<'_> {
        Rx(self)
    }
}

struct LinkPermit<'a> {
    slot: MutexGuard<'a, Vec<u8>>,
    link: &'a Link,
}

impl AsMut<[u8]> for LinkPermit<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.slot
    }
}

impl Permit for LinkPermit<'_> {
    fn send(self, len: usize) -> Result<(), SendError> {
        if !self.link.size.contains(len) {
            return Err(SendError::BadLength {
                len,
                size: self.link.size,
            });
        }
        if self.link.refusing.load(Ordering::Relaxed) {
            return Err(SendError::Dropped);
        }
        self.link
            .frames
            .lock()
            .unwrap()
            .push_back(Bytes::copy_from_slice(&self.slot[..len]));
        Ok(())
    }
}

impl FrameTx for Link {
    type Permit<'a> = LinkPermit<'a>;

    fn frame_size(&self) -> FrameSize {
        self.size
    }

    fn poll_reserve(&self, _: &mut Context<'_>) -> Poll<LinkPermit<'_>> {
        match self.slot.try_lock() {
            Ok(slot) => Poll::Ready(LinkPermit { slot, link: self }),
            Err(_) => Poll::Pending,
        }
    }
}

struct Rx<'a>(&'a Link);

impl Stream for Rx<'_> {
    type Item = Bytes;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Bytes>> {
        Poll::Ready(self.0.frames.lock().unwrap().pop_front())
    }
}

impl FrameRx for Rx<'_> {
    fn frame_size(&self) -> FrameSize {
        self.0.size
    }
}

// A tiny waker that raises a flag when woken.
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
        self.0.store(true, Ordering::SeqCst);
    }
}

// The output of a future that is ready on its first poll.
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(noop_waker_ref()))
    {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the future was not ready"),
    }
}

fn fixed(len: usize) -> FrameSize {
    FrameSize::fixed(len).unwrap()
}

// A sender over `link`, with a default configuration apart from the window:
// its PDU size and framing come from the link.
fn sender_over(link: &Link) -> frame::Sender<&Link> {
    frame::Sender::new(sender_config(1500, WINDOW), link, 0).unwrap()
}

// A bundle whose first byte marks it as BPv7, so a sender may send it bare.
fn bpv7(len: usize) -> Bytes {
    let mut data = vec![0x42; len];
    data[0] = 0x9f;
    Bytes::from(data)
}

type Sent = (Vec<Carried>, Result<(), SendError>);

fn poll_send(sender: &mut frame::Sender<&Link>, carried: &mut CarriedList) -> Poll<Sent> {
    sender
        .poll_send(&mut Context::from_waker(noop_waker_ref()), carried)
        .map(|result| (carried.to_vec(), result))
}

fn poll_flush(sender: &mut frame::Sender<&Link>, carried: &mut CarriedList) -> Poll<Sent> {
    sender
        .poll_flush(&mut Context::from_waker(noop_waker_ref()), carried)
        .map(|result| (carried.to_vec(), result))
}

// The owner's drain, run until the sender has nothing more that is ready:
// what each frame carried and whether the service took it.
fn drain(sender: &mut frame::Sender<&Link>) -> Vec<Sent> {
    let mut sent = Vec::new();
    let mut carried = CarriedList::default();
    while let Poll::Ready(frame) = poll_send(sender, &mut carried) {
        sent.push(frame);
    }
    sent
}

fn completed(sent: &[Sent]) -> Vec<SendId> {
    sent.iter()
        .flat_map(|(carried, _)| carried)
        .filter(|c| c.completes)
        .map(|c| c.id)
        .collect()
}

// Every event the receiver over `link`'s receive half yields.
fn receive(link: &Link) -> Vec<ReceiverEvent> {
    let mut receiver = frame::Receiver::new(receiver(WINDOW, 1000), link.rx());
    assert_eq!(receiver.frame_size(), link.size);
    let mut events = Vec::new();
    while let Some(event) = ready(receiver.next()) {
        events.push(event);
    }
    events
}

#[test]
fn a_fixed_service_is_fixed_size_framing_and_every_frame_is_one_pdu() {
    let sized = SenderConfig::default().with_frame_size(fixed(64)).unwrap();
    assert_eq!(sized.pdu_size, PduSize::new(64).unwrap());
    assert_eq!(sized.link_framing, LinkFraming::FixedSize);
    let link = Link::new(fixed(64));
    let mut sender = sender_over(&link);
    assert_eq!(sender.frame_size(), fixed(64));

    let small = sender.enqueue(bundle(20), SendOptions::default()).unwrap();
    let large = sender
        .enqueue(patterned(150), SendOptions::default())
        .unwrap();
    let sent = drain(&mut sender);

    assert_eq!(completed(&sent), vec![small, large]);
    assert!(sent.iter().all(|(_, result)| result.is_ok()));
    assert!(!sender.has_pending());
    let frames = link.frames();
    assert_eq!(frames.len(), 4);
    assert!(frames.iter().all(|frame| frame.len() == 64));

    assert_eq!(
        receive(&link),
        vec![
            received_with(bundle(20), vec![]),
            reassembled_with(0, patterned(150), vec![HintItem::BundleLength(150)]),
        ]
    );
}

#[test]
fn a_ranged_service_is_variable_framing_padded_to_its_minimum() {
    let size = FrameSize::new(46, 200).unwrap();
    let sized = SenderConfig::default().with_frame_size(size).unwrap();
    assert_eq!(sized.pdu_size, PduSize::new(200).unwrap());
    assert_eq!(
        sized.link_framing,
        LinkFraming::Variable {
            bundle_framing: BundleFraming::Message,
            min_pdu_len: 46,
        }
    );
    let link = Link::new(size);
    // The caller's bundle framing is kept; the floor is the service's.
    let mut config = sender_config(1500, WINDOW);
    config.link_framing = LinkFraming::variable(BundleFraming::Bare);
    let mut sender = frame::Sender::new(config, &link, 0).unwrap();

    sender.enqueue(bpv7(30), SendOptions::default()).unwrap();
    sender.enqueue(bpv7(100), SendOptions::default()).unwrap();
    drain(&mut sender);

    let frames = link.frames();
    assert_eq!(frames.len(), 2);
    // Too short to go bare under the floor, so a Bundle Message padded to
    // the floor; then the bundle's own bytes, which already clear it.
    assert_eq!(frames[0].len(), 46);
    assert_ne!(frames[0][0], 0x9f);
    assert_eq!(frames[1], bpv7(100));
    assert!(frames.iter().all(|frame| size.contains(frame.len())));

    assert_eq!(
        receive(&link),
        vec![
            received_with(bpv7(30), vec![]),
            received_with(bpv7(100), vec![]),
        ]
    );
}

#[test]
fn a_floor_above_the_service_minimum_is_kept() {
    let link = Link::new(FrameSize::new(46, 200).unwrap());
    let mut config = sender_config(1500, WINDOW);
    config.link_framing = LinkFraming::Variable {
        bundle_framing: BundleFraming::Message,
        min_pdu_len: 80,
    };
    let mut sender = frame::Sender::new(config, &link, 0).unwrap();

    sender.enqueue(bundle(20), SendOptions::default()).unwrap();
    drain(&mut sender);

    assert_eq!(link.frames()[0].len(), 80);
}

#[test]
fn a_service_too_small_for_a_pdu_is_refused_at_construction() {
    let link = Link::new(fixed(3));
    let Err(err) = frame::Sender::new(sender_config(1500, WINDOW), &link, 0) else {
        panic!("a frame shorter than a message header is no PDU");
    };
    assert_eq!(
        err,
        OutOfRange {
            name: "PDU size",
            value: 3,
            min: PduSize::MIN.get() as u64,
            max: Some(PduSize::MAX.get() as u64),
        }
    );
}

#[test]
fn poll_send_waits_for_a_pdu_and_then_for_a_frame() {
    let link = Link::new(fixed(64));
    let mut sender = sender_over(&link);
    let mut list = CarriedList::default();
    let (flag, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);

    // Nothing to send: parked on the sender.
    assert!(sender.poll_send(&mut cx, &mut list).is_pending());
    assert!(!flag.raised());
    let id = sender.enqueue(bundle(20), SendOptions::default()).unwrap();
    assert!(flag.raised());

    // Something to send but no frame: parked on the half, nothing packed.
    let held = link.hold();
    assert!(sender.poll_send(&mut cx, &mut list).is_pending());
    assert!(sender.is_outstanding(id));
    drop(held);

    assert_eq!(
        poll_send(&mut sender, &mut list),
        Poll::Ready((vec![carried(id, true)], Ok(())))
    );
    assert_eq!(link.frames().len(), 1);
}

#[test]
fn a_bundle_cancelled_before_its_frame_sends_nothing() {
    let link = Link::new(fixed(64));
    let mut sender = sender_over(&link);
    let mut carried = CarriedList::default();

    let id = sender.enqueue(bundle(20), SendOptions::default()).unwrap();
    assert!(sender.cancel(id));
    assert!(poll_send(&mut sender, &mut carried).is_pending());
    assert!(link.frames().is_empty());
    // The frame went back: the next PDU gets it.
    sender.enqueue(bundle(20), SendOptions::default()).unwrap();
    assert!(poll_send(&mut sender, &mut carried).is_ready());
}

#[test]
fn a_flush_sends_what_a_waiting_transfer_has_buffered() {
    let link = Link::new(fixed(64));
    let mut sender = sender_over(&link);
    let mut list = CarriedList::default();
    let data = patterned(150);

    // Less than a segment pushed: the transfer waits on its producer, and
    // the drain has nothing to send.
    let mut handle = sender.begin(data.len(), SendOptions::default()).unwrap();
    sender.push(&mut handle, data.slice(..20)).unwrap();
    assert!(poll_send(&mut sender, &mut list).is_pending());

    // The CLA's clock says the producer has gone quiet.
    assert_eq!(
        poll_flush(&mut sender, &mut list),
        Poll::Ready((vec![carried(handle.id(), false)], Ok(())))
    );
    assert_eq!(link.frames().len(), 1);
    assert_eq!(link.frames()[0].len(), 64);

    // Nothing left to flush: the frame goes back unsent.
    assert_eq!(
        poll_flush(&mut sender, &mut list),
        Poll::Ready((vec![], Ok(())))
    );
    assert_eq!(link.frames().len(), 1);

    sender.push(&mut handle, data.slice(20..)).unwrap();
    let id = sender.finish(handle).unwrap();
    assert_eq!(completed(&drain(&mut sender)), vec![id]);
    assert_eq!(
        receive(&link),
        vec![reassembled_with(0, data, vec![HintItem::BundleLength(150)])]
    );
}

#[test]
fn a_frame_the_service_refuses_names_the_bundles_it_carried() {
    let link = Link::new(fixed(64));
    let mut sender = sender_over(&link);
    link.refuse(true);

    let id = sender.enqueue(bundle(20), SendOptions::default()).unwrap();
    // The PDU was packed, so the sender has let the bundle go; the refusal
    // and the carried list are what the CLA fails it from.
    assert_eq!(
        drain(&mut sender),
        vec![(vec![carried(id, true)], Err(SendError::Dropped))]
    );
    assert!(!sender.is_outstanding(id));
    assert!(link.frames().is_empty());
    assert_eq!(receive(&link), Vec::<Event>::new());
}

#[test]
fn poll_ready_parks_a_producer_until_the_window_opens() {
    let link = Link::new(fixed(64));
    let mut sender = frame::Sender::new(sender_config(1500, WINDOW), &link, 0).unwrap();
    let (flag, waker) = Flag::waker();
    let mut cx = Context::from_waker(&waker);

    // A window of segmented bundles, none yet drained.
    for _ in 0..WINDOW {
        assert_eq!(sender.poll_ready(&mut cx), Poll::Ready(()));
        sender
            .enqueue(patterned(150), SendOptions::default())
            .unwrap();
    }
    assert_eq!(sender.poll_ready(&mut cx), Poll::Pending);
    assert!(!flag.raised());

    drain(&mut sender);
    assert!(flag.raised());
    assert_eq!(sender.poll_ready(&mut cx), Poll::Ready(()));
}
