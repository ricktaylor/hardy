#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
/*!
Stackable transmit and receive halves for the frame services below Hardy's
convergence layers.

A frame service carries bounded, whole-or-nothing units: AOS transfer
frames, Encapsulation and Space Packets, Ethernet frames, UDP datagrams,
QUIC DATAGRAM frames.  Protocol engines such as BTP-U and LTP turn bundles
into frames and back, and below them sits a stack of such services.  This
crate is the one interface every layer of that stack offers to the layer
above and takes from the layer below, so that any layer stacks onto any
other whose frames it can use, and a stack is a type assembled where it is
deployed.

The crate holds the traits and nothing else: no layer, no link, no runtime.
It is `#![no_std]` and depends only on `core`, `alloc` (through [`bytes`]),
[`futures_core`] and `thiserror`.

# Key Types

- [`FrameTx`] — the transmit half: reserve room for one frame, write it in
  place, send it.
- [`Permit`] — the room a reservation lends, and the send that consumes it.
- [`FrameRx`] — the receive half: a pull stream of frames.
- [`FrameSize`] — the range of frame lengths a half carries.
- [`SendError`] — why a send was refused.

# Feature Flags

- `critical-section` — builds on targets without native atomic
  compare-and-swap by switching `bytes` to `portable-atomic`'s
  critical-section fallback.
*/

extern crate alloc;

#[cfg(target_has_atomic = "ptr")]
use alloc::sync::Arc;
use core::{
    fmt::{self, Display, Formatter},
    future::{Future, poll_fn},
    ops::RangeInclusive,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_core::Stream;

/// The range of frame lengths a half carries: every frame is at least
/// [`min`](Self::min) and at most [`max`](Self::max) bytes long, and a
/// frame carries at least one byte.
///
/// Two cases are common enough to name: a *fixed* service, whose minimum
/// and maximum are equal (AOS transfer frames), and a *maximum* service,
/// whose minimum is 1 (UDP datagrams).  Some services have a real minimum:
/// an Ethernet payload is at least 46 bytes, and a shorter one is padded
/// by the MAC with padding the receiver can't tell from data, so the layer
/// above pads to the minimum itself, with padding it can tell apart.
///
/// A size is fixed for a half's life.  A layer reads the size of the half
/// below it once, when it is built, and derives its own from it with
/// [`behind`](Self::behind); a stack whose sizes don't fit is refused when
/// it is assembled, not at its first frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameSize {
    min: usize,
    max: usize,
}

impl FrameSize {
    /// Frames of `min` to `max` bytes, or `None` unless `1 <= min <= max`.
    pub const fn new(min: usize, max: usize) -> Option<Self> {
        if 1 <= min && min <= max {
            Some(Self { min, max })
        } else {
            None
        }
    }

    /// Frames of exactly `len` bytes, or `None` if `len` is 0.
    pub const fn fixed(len: usize) -> Option<Self> {
        Self::new(len, len)
    }

    /// Frames of 1 to `max` bytes, or `None` if `max` is 0.
    pub const fn up_to(max: usize) -> Option<Self> {
        Self::new(1, max)
    }

    /// The shortest frame the half carries.
    pub const fn min(&self) -> usize {
        self.min
    }

    /// The longest frame the half carries, and the length of the slice a
    /// [`Permit`] lends.
    pub const fn max(&self) -> usize {
        self.max
    }

    /// Whether every frame is the same length.
    pub const fn is_fixed(&self) -> bool {
        self.min == self.max
    }

    /// Whether a half of this size carries every frame a half of size
    /// `other` can emit: whether `other` lies within this range.
    pub const fn covers(&self, other: &FrameSize) -> bool {
        self.min <= other.min && other.max <= self.max
    }

    /// Whether a frame of `len` bytes is within the range.
    pub const fn contains(&self, len: usize) -> bool {
        self.min <= len && len <= self.max
    }

    /// The size a layer offers above when it puts a header of `header`
    /// bytes in front of each frame it sends through a half of this size,
    /// or `None` if the header leaves no room: the maximum shrinks by the
    /// header, and so does the minimum, to no less than one byte.
    pub const fn behind(&self, header: usize) -> Option<Self> {
        if header >= self.max {
            return None;
        }
        let min = self.min.saturating_sub(header);
        Self::new(if min == 0 { 1 } else { min }, self.max - header)
    }
}

impl From<FrameSize> for RangeInclusive<usize> {
    fn from(size: FrameSize) -> Self {
        size.min..=size.max
    }
}

impl Display for FrameSize {
    /// `1115` for a fixed size, `46..=1500` otherwise.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.is_fixed() {
            write!(f, "{}", self.max)
        } else {
            write!(f, "{}..={}", self.min, self.max)
        }
    }
}

/// Why a send was refused.
///
/// Closing is not an error: a half that has closed for good grants no more
/// reservations, and its user learns of the closure from its receive stream
/// ending or from its own shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// A layer below refused this one frame by its own policy, or lost its
    /// link after granting the reservation, and the service carries on.
    #[error("frame dropped")]
    Dropped,
    /// The frame's length is outside the range the service carries.
    #[error("a frame of {len} bytes is outside the {size} bytes the service carries")]
    BadLength {
        /// The length the caller asked to send.
        len: usize,
        /// The range the service carries.
        size: FrameSize,
    },
}

/// Room for one frame, lent by [`FrameTx::poll_reserve`].
///
/// The slice [`as_mut`](AsMut::as_mut) yields is exactly
/// [`FrameSize::max`] bytes long and its contents are unspecified, so the
/// caller writes every byte it sends, padding included.  The memory is
/// whatever the bottom of the stack has: a slot in a shared-memory ring, a
/// DMA buffer, a buffer a channel-backed layer allocated.  A layer that
/// adds a header lends the slice behind it and writes the header in front
/// when the frame is sent, so nothing is copied on the way down.
///
/// A permit is used at once: reserve, write and send with no `.await` in
/// between, so that no layer below waits on a caller that has gone off to
/// wait for something else while holding room.  Dropping a permit unsent
/// returns its room and sends nothing.  Permits are therefore not `Send`:
/// a type that holds room in a queue carries a `PhantomData<*const ()>`,
/// and a permit that wraps it inherits that, so holding one across an
/// `.await` in a task that must be `Send` is a compile error.
///
/// ```compile_fail
/// use core::{future::Future, marker::PhantomData};
///
/// use hardy_frame::{Permit, SendError};
///
/// struct Slot(Vec<u8>, PhantomData<*const ()>);
///
/// impl AsMut<[u8]> for Slot {
///     fn as_mut(&mut self) -> &mut [u8] {
///         &mut self.0
///     }
/// }
///
/// impl Permit for Slot {
///     fn send(self, _len: usize) -> Result<(), SendError> {
///         Ok(())
///     }
/// }
///
/// fn spawn(_task: impl Future<Output = ()> + Send) {}
///
/// spawn(async {
///     let permit = Slot(vec![0; 4], PhantomData);
///     async {}.await; // held across an await: the task is no longer `Send`
///     permit.send(1).unwrap();
/// });
/// ```
///
/// Used at once, the same permit is fine in the same task:
///
/// ```
/// # use core::{future::Future, marker::PhantomData};
/// # use hardy_frame::{Permit, SendError};
/// # struct Slot(Vec<u8>, PhantomData<*const ()>);
/// # impl AsMut<[u8]> for Slot {
/// #     fn as_mut(&mut self) -> &mut [u8] {
/// #         &mut self.0
/// #     }
/// # }
/// # impl Permit for Slot {
/// #     fn send(self, _len: usize) -> Result<(), SendError> {
/// #         Ok(())
/// #     }
/// # }
/// # fn spawn(_task: impl Future<Output = ()> + Send) {}
/// spawn(async {
///     async {}.await;
///     let mut permit = Slot(vec![0; 4], PhantomData);
///     permit.as_mut()[0] = 1;
///     permit.send(1).unwrap();
/// });
/// ```
pub trait Permit: AsMut<[u8]> {
    /// Send the first `len` bytes of the slice as one frame.
    ///
    /// Never waits.  `Ok` means a layer below has accepted the frame into
    /// its queue, not that it has been transmitted; frames leave in the
    /// order their permits are sent.  A `len` outside the service's
    /// [`FrameSize`] is refused as [`SendError::BadLength`], and a frame
    /// the layer below refuses or loses is [`SendError::Dropped`].
    fn send(self, len: usize) -> Result<(), SendError>;
}

/// The transmit half of a frame service.
///
/// Transmitting is two steps.  [`poll_reserve`](Self::poll_reserve) waits
/// for room for one frame and lends the memory the frame is written into;
/// it is the only wait, and waiting is backpressure.  [`Permit::send`]
/// consumes the permit with the frame's length and never waits.  The split
/// exists for packers: BTP-U packs a PDU only once it holds a permit, so a
/// packed PDU always has somewhere to go.  [`reserve`](Self::reserve) is
/// the same wait as a future, and [`send`](Self::send) does both steps for
/// a caller that already has its frame.
///
/// The wait is a poll method rather than an `async fn` so that an engine
/// which owns a half can be a plain state machine, polled from one task
/// with no future of its own to store, no lock and no spawn; a layer over
/// another half delegates the poll.
///
/// Admission is first come, first served: blocked reservations are granted
/// in the order they were first polled, and frames leave in the order their
/// permits are sent.  Dropping a reservation before it is granted holds
/// nothing.  While the link is down, reservations wait, which pauses every
/// packer above with no policy of its own; a half that has closed for good
/// never grants again.
///
/// Stacks share a transmit half between tasks, so it is `Send + Sync` and
/// is also implemented for `&T` and, on targets with pointer-width
/// atomics, `Arc<T>`; the futures it returns are `Send`, and its permits
/// deliberately are not (see [`Permit`]).
pub trait FrameTx: Send + Sync {
    /// The room a reservation lends.
    type Permit<'a>: Permit
    where
        Self: 'a;

    /// The range of frame lengths the half carries.
    fn frame_size(&self) -> FrameSize;

    /// Poll for room for one frame.
    ///
    /// `Pending` while the queue below is full or the link is down, with
    /// the task woken when room appears; a half that has closed for good
    /// stays `Pending`.  Never fails.  Blocked reservations are granted in
    /// the order they were first polled, and a task that stops polling
    /// before it is granted holds nothing.
    fn poll_reserve(&self, cx: &mut Context<'_>) -> Poll<Self::Permit<'_>>;

    /// Wait for room for one frame: [`poll_reserve`](Self::poll_reserve)
    /// as a future.
    fn reserve(&self) -> impl Future<Output = Self::Permit<'_>> + Send {
        poll_fn(move |cx| self.poll_reserve(cx))
    }

    /// Reserve room, copy `frame` into it and send it: [`reserve`](Self::reserve)
    /// then [`Permit::send`], for a caller that already has its frame.
    ///
    /// A `frame` outside the half's [`FrameSize`] is refused as
    /// [`SendError::BadLength`] without waiting for room.
    fn send(&self, frame: &[u8]) -> impl Future<Output = Result<(), SendError>> + Send {
        async move {
            let size = self.frame_size();
            if !size.contains(frame.len()) {
                return Err(SendError::BadLength {
                    len: frame.len(),
                    size,
                });
            }
            let mut permit = self.reserve().await;
            permit.as_mut()[..frame.len()].copy_from_slice(frame);
            permit.send(frame.len())
        }
    }
}

impl<T: FrameTx + ?Sized> FrameTx for &T {
    type Permit<'a>
        = T::Permit<'a>
    where
        Self: 'a;

    fn frame_size(&self) -> FrameSize {
        (**self).frame_size()
    }

    fn poll_reserve(&self, cx: &mut Context<'_>) -> Poll<Self::Permit<'_>> {
        (**self).poll_reserve(cx)
    }

    fn send(&self, frame: &[u8]) -> impl Future<Output = Result<(), SendError>> + Send {
        (**self).send(frame)
    }
}

// `Arc` needs pointer-width atomics, which some targets lack (Cortex-M0).
#[cfg(target_has_atomic = "ptr")]
impl<T: FrameTx + ?Sized> FrameTx for Arc<T> {
    type Permit<'a>
        = T::Permit<'a>
    where
        Self: 'a;

    fn frame_size(&self) -> FrameSize {
        (**self).frame_size()
    }

    fn poll_reserve(&self, cx: &mut Context<'_>) -> Poll<Self::Permit<'_>> {
        (**self).poll_reserve(cx)
    }

    fn send(&self, frame: &[u8]) -> impl Future<Output = Result<(), SendError>> + Send {
        (**self).send(frame)
    }
}

/// The receive half of a frame service: a pull stream of frames.
///
/// Frames arrive in link order, each within the half's [`FrameSize`].  A
/// frame is consumed only when a poll returns it, so dropping a pending
/// poll loses nothing; the end of the stream means the service has closed
/// for good; and a consumer that stops polling stops taking frames, which
/// is receive backpressure.  A layer that can't hold frames drops them by
/// its own policy and counts them, and loss is not signalled here: a
/// consumer that cares detects it from its own sequence numbers and timers.
///
/// A frame may be a view of a lower layer's buffer.  A consumer that keeps
/// a frame's bytes past its next poll copies them, or accepts that it pins
/// the buffer behind them.
///
/// The half has no `Send` bound of its own: whoever moves one into a
/// spawned task requires it there.
pub trait FrameRx: Stream<Item = Bytes> {
    /// The range of frame lengths the half carries.
    fn frame_size(&self) -> FrameSize;
}

impl<R: FrameRx + Unpin + ?Sized> FrameRx for &mut R {
    fn frame_size(&self) -> FrameSize {
        (**self).frame_size()
    }
}
