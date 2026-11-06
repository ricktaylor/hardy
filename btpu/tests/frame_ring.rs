//! BTP-U packing straight into the slots of a frame ring through the frame
//! layer, with no allocation per frame: the ring's permit is a slot, and
//! the sender packs each PDU into it in place.  Alone in its binary so the
//! allocation count sees only this test.

#![cfg(feature = "frame")]

mod common;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::VecDeque,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::{StreamExt, task::noop_waker_ref};
use hardy_btpu::{
    codec::hint::HintItem,
    frame,
    sender::{BundleFraming, CarriedList, LinkFraming, SendOptions},
};
use hardy_frame::{FrameRx, FrameSize, FrameTx, Permit, SendError};

use common::{bundle, patterned, reassembled_with, received_with, receiver, sender_config};

// `data` as the BPA hands bundles over: already shared.  A `Bytes` fresh
// from a `Vec` is promoted to a shared buffer, with one small allocation,
// the first time it is cloned or sliced, which cutting its first segment
// would otherwise do inside the drain.
fn shared(data: Bytes) -> Bytes {
    data.slice(..)
}

// Counts every allocation in the process.
struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const SLOT: usize = 64;
const SLOTS: usize = 4;

// A frame ring: fixed slots the producer fills in place and publishes by
// length, as a shared-memory ring to a link pusher would be.  Everything is
// allocated when the ring is built, so filling and publishing allocate
// nothing.
struct Ring {
    size: FrameSize,
    slots: Vec<Mutex<Vec<u8>>>,
    free: Mutex<VecDeque<usize>>,
    published: Mutex<VecDeque<(usize, usize)>>,
}

impl Ring {
    fn new(size: FrameSize) -> Self {
        assert_eq!(size.max(), SLOT);
        Self {
            size,
            slots: (0..SLOTS).map(|_| Mutex::new(vec![0; SLOT])).collect(),
            free: Mutex::new((0..SLOTS).collect()),
            published: Mutex::new(VecDeque::with_capacity(SLOTS)),
        }
    }

    // The consumer's side: takes every published frame off the ring, in
    // order, freeing its slot.  This copies, and is outside what is counted.
    fn consume(&self) -> Vec<Bytes> {
        let mut frames = Vec::new();
        while let Some((index, len)) = self.published.lock().unwrap().pop_front() {
            frames.push(Bytes::copy_from_slice(
                &self.slots[index].lock().unwrap()[..len],
            ));
            self.free.lock().unwrap().push_back(index);
        }
        frames
    }
}

struct Slot<'a> {
    index: usize,
    buf: MutexGuard<'a, Vec<u8>>,
    ring: &'a Ring,
    sent: bool,
}

impl AsMut<[u8]> for Slot<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }
}

impl Permit for Slot<'_> {
    fn send(mut self, len: usize) -> Result<(), SendError> {
        if !self.ring.size.contains(len) {
            return Err(SendError::BadLength {
                len,
                size: self.ring.size,
            });
        }
        self.ring
            .published
            .lock()
            .unwrap()
            .push_back((self.index, len));
        self.sent = true;
        Ok(())
    }
}

impl Drop for Slot<'_> {
    // A slot dropped unsent goes back to the free list.
    fn drop(&mut self) {
        if !self.sent {
            self.ring.free.lock().unwrap().push_back(self.index);
        }
    }
}

impl FrameTx for Ring {
    type Permit<'a> = Slot<'a>;

    fn frame_size(&self) -> FrameSize {
        self.size
    }

    fn poll_reserve(&self, _: &mut Context<'_>) -> Poll<Slot<'_>> {
        let Some(index) = self.free.lock().unwrap().pop_front() else {
            return Poll::Pending;
        };
        Poll::Ready(Slot {
            index,
            buf: self.slots[index].lock().unwrap(),
            ring: self,
            sent: false,
        })
    }
}

// The frames the consumer took off the ring, as a receive half.
struct Frames {
    size: FrameSize,
    frames: VecDeque<Bytes>,
}

impl futures::Stream for Frames {
    type Item = Bytes;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Bytes>> {
        Poll::Ready(self.frames.pop_front())
    }
}

impl FrameRx for Frames {
    fn frame_size(&self) -> FrameSize {
        self.size
    }
}

// Drains the sender into the ring until it parks, consuming the ring as
// it fills, and counting the allocations the drain itself makes.  Returns
// the frames in the order they were published, and that count.
fn drain(sender: &mut frame::Sender<&Ring>, ring: &Ring) -> (Vec<Bytes>, usize) {
    let mut carried = CarriedList::default();
    let mut cx = Context::from_waker(noop_waker_ref());
    let mut frames = Vec::new();
    let mut allocations = 0;
    loop {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let polled = sender.poll_send(&mut cx, &mut carried);
        allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
        match polled {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(err)) => panic!("the ring refused a frame: {err}"),
            // Parked on the ring, which is full, or on the sender, which is
            // empty.
            Poll::Pending if ring.free.lock().unwrap().is_empty() => {
                frames.extend(ring.consume());
            }
            Poll::Pending => {
                frames.extend(ring.consume());
                return (frames, allocations);
            }
        }
    }
}

#[test]
fn packing_into_ring_slots_allocates_nothing_per_frame() {
    // A fixed service of the slot size: every PDU fills a slot.
    let ring = Ring::new(FrameSize::fixed(SLOT).unwrap());
    let mut sender = frame::Sender::new(sender_config(SLOT, 4), &ring, 0).unwrap();

    // Queue first, where a transfer allocates its bookkeeping.
    for data in [bundle(20), patterned(150), bundle(30), patterned(300)] {
        sender
            .enqueue(shared(data), SendOptions::default())
            .unwrap();
    }

    let (frames, allocations) = drain(&mut sender, &ring);
    assert!(
        frames.len() > SLOTS,
        "the ring wrapped: {} frames",
        frames.len()
    );
    assert_eq!(
        allocations,
        0,
        "allocations while packing {} frames",
        frames.len()
    );
    assert!(frames.iter().all(|frame| frame.len() == SLOT));
    let mut receiver = frame::Receiver::new(
        receiver(4, 1000),
        Frames {
            size: ring.size,
            frames: frames.into(),
        },
    );
    let mut events = Vec::new();
    while let Poll::Ready(Some(event)) =
        receiver.poll_next_unpin(&mut Context::from_waker(noop_waker_ref()))
    {
        events.push(event);
    }
    assert_eq!(
        events,
        vec![
            received_with(bundle(20), vec![]),
            reassembled_with(0, patterned(150), vec![HintItem::BundleLength(150)]),
            received_with(bundle(30), vec![]),
            reassembled_with(1, patterned(300), vec![HintItem::BundleLength(300)]),
        ]
    );

    // A ranged service over the same slots: bare frames go in place too.
    let ring = Ring::new(FrameSize::new(46, SLOT).unwrap());
    let mut config = sender_config(SLOT, 4);
    config.link_framing = LinkFraming::variable(BundleFraming::Bare);
    let mut sender = frame::Sender::new(config, &ring, 0).unwrap();
    let mut bare = vec![0x42; 50];
    bare[0] = 0x9f;
    for data in [Bytes::from(bare.clone()), bundle(20)] {
        sender
            .enqueue(shared(data), SendOptions::default())
            .unwrap();
    }

    let (frames, allocations) = drain(&mut sender, &ring);
    assert_eq!(allocations, 0);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], Bytes::from(bare));
    assert_eq!(frames[1].len(), 46);
}
