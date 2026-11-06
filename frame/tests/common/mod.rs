//! Fixtures shared between the integration-test binaries: an in-memory
//! link with one slot of frame memory, a length-prefix header layer over
//! any transmit half, a receive half over a list of frames, and hand
//! polling without a runtime.

// Each test binary uses its own subset of these helpers, so per-binary
// unused ones are expected.
#![allow(dead_code)]

use std::{
    collections::VecDeque,
    future::Future,
    pin::{Pin, pin},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::task::noop_waker_ref;
use hardy_frame::{FrameRx, FrameSize, FrameTx, Permit, SendError};

// Polls `future` once with a waker that does nothing.
pub fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
    let mut future = pin!(future);
    future
        .as_mut()
        .poll(&mut Context::from_waker(noop_waker_ref()))
}

// The output of a future that is ready on its first poll.
pub fn ready<F: Future>(future: F) -> F::Output {
    match poll_once(future) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the future was not ready"),
    }
}

pub fn size(min: usize, max: usize) -> FrameSize {
    FrameSize::new(min, max).unwrap()
}

pub fn fixed(len: usize) -> FrameSize {
    FrameSize::fixed(len).unwrap()
}

// A link with one slot of frame memory, as a ring with one entry would be.
// A reservation is granted while the slot is free and nobody has closed the
// link; sending copies the frame out of the slot onto `sent`, in order,
// unless the link has been told to refuse every frame.
pub struct Slot {
    size: FrameSize,
    slot: Mutex<Vec<u8>>,
    sent: Mutex<Vec<Vec<u8>>>,
    closed: AtomicBool,
    refusing: AtomicBool,
}

impl Slot {
    pub fn new(size: FrameSize) -> Self {
        Self {
            size,
            slot: Mutex::new(vec![0; size.max()]),
            sent: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            refusing: AtomicBool::new(false),
        }
    }

    // The frames sent so far, in the order they left.
    pub fn sent(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap().clone()
    }

    // Closes the link for good: no reservation is granted again.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    // Whether every frame sent from now on is refused as dropped.
    pub fn refuse(&self, refusing: bool) {
        self.refusing.store(refusing, Ordering::Relaxed);
    }
}

pub struct SlotPermit<'a> {
    slot: MutexGuard<'a, Vec<u8>>,
    link: &'a Slot,
}

impl AsMut<[u8]> for SlotPermit<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.slot
    }
}

impl Permit for SlotPermit<'_> {
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
            .sent
            .lock()
            .unwrap()
            .push(self.slot[..len].to_vec());
        Ok(())
    }
}

impl FrameTx for Slot {
    type Permit<'a> = SlotPermit<'a>;

    fn frame_size(&self) -> FrameSize {
        self.size
    }

    fn poll_reserve(&self, _: &mut Context<'_>) -> Poll<SlotPermit<'_>> {
        if self.closed.load(Ordering::Relaxed) {
            return Poll::Pending;
        }
        match self.slot.try_lock() {
            Ok(slot) => Poll::Ready(SlotPermit { slot, link: self }),
            Err(_) => Poll::Pending,
        }
    }
}

// A layer that puts a two-byte big-endian length in front of each frame,
// written in place when the frame is sent.
pub const HEADER: usize = 2;

pub struct Header<T> {
    below: T,
    size: FrameSize,
}

impl<T: FrameTx> Header<T> {
    // `None` if the half below has no room behind the header.
    pub fn new(below: T) -> Option<Self> {
        let size = below.frame_size().behind(HEADER)?;
        Some(Self { below, size })
    }
}

pub struct HeaderPermit<P>(P);

impl<P: Permit> AsMut<[u8]> for HeaderPermit<P> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.0.as_mut()[HEADER..]
    }
}

impl<P: Permit> Permit for HeaderPermit<P> {
    fn send(mut self, len: usize) -> Result<(), SendError> {
        let header = u16::try_from(len).expect("a frame fits the length field");
        self.0.as_mut()[..HEADER].copy_from_slice(&header.to_be_bytes());
        self.0.send(len + HEADER)
    }
}

impl<T: FrameTx> FrameTx for Header<T> {
    type Permit<'a>
        = HeaderPermit<T::Permit<'a>>
    where
        Self: 'a;

    fn frame_size(&self) -> FrameSize {
        self.size
    }

    fn poll_reserve(&self, cx: &mut Context<'_>) -> Poll<Self::Permit<'_>> {
        self.below.poll_reserve(cx).map(HeaderPermit)
    }
}

// A receive half that yields a fixed list of frames, then ends.
pub struct Frames {
    size: FrameSize,
    frames: VecDeque<Bytes>,
}

impl Frames {
    pub fn new(size: FrameSize, frames: impl IntoIterator<Item = Bytes>) -> Self {
        Self {
            size,
            frames: frames.into_iter().collect(),
        }
    }
}

impl futures::Stream for Frames {
    type Item = Bytes;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Bytes>> {
        Poll::Ready(self.frames.pop_front())
    }
}

impl FrameRx for Frames {
    fn frame_size(&self) -> FrameSize {
        self.size
    }
}
