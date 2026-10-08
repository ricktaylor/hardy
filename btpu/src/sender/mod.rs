//! The sending end: queues bundles, allocates their transfer numbers within
//! the Section 5 window, and packs their messages into PDUs.

mod config;
mod error;
mod id;
mod options;
mod pack;
mod pdu;
mod queue;
mod segmenter;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    vec::Vec,
};
#[cfg(feature = "tower")]
use core::task::Waker;
use core::{fmt, iter::from_fn};

use bytes::{Bytes, BytesMut};
#[cfg(feature = "rand")]
use rand_core::{Rng, TryRng};
use smallvec::SmallVec;

pub use self::{
    config::{
        BundleFraming, LinkFraming, PduSize, SegmentCutStrategy, SendQueueHighWatermark,
        SenderConfig,
    },
    error::{Error, Result},
    id::{SendHandle, SendId, SendKind},
    options::{NextPduOptions, SendOptions, SendRequest},
    pdu::{Carried, CarriedList, Pdu},
};
use self::{
    id::LocalId,
    queue::{Assembly, Order, QueueEntry, Queued, Seq},
    segmenter::QueuedTransfer,
};
use crate::{
    codec::{
        encoded_message_len,
        header::HEADER_SIZE,
        hint::{HintItem, HintType},
        message::{FrameKind, Message, SEGMENT_FRAMING, frame_kind},
    },
    owner::Owner,
    transfer::TransferNumberAllocator,
};

/// Where [`Sender::find_unsegmented`] found a bundle: its position in the
/// send queue, or among the bundles still being pushed.
enum Unsegmented {
    Queued(Seq),
    Assembling,
}

/// Manages outbound BTP-U transfers, segmentation, and PDU packing.
///
/// The sender is convergence-layer agnostic: a CLA calls [`Sender::enqueue`] to
/// submit bundles and [`Sender::next_pdu`] to obtain packed PDUs ready for
/// transmission, each listing the bundles it carries.
///
/// # Transfer window
///
/// A segmented bundle takes a Section 5 window slot when it is enqueued and
/// gives it back when its Transfer End is packed into a PDU by
/// [`Self::next_pdu`]: a unidirectional link offers no acknowledgement to
/// anchor an explicit completion call to, and once the End has left the
/// queue the sender has nothing further to emit for the transfer.  The only
/// other way out of the window is [`Self::cancel`].  The window is enforced
/// on the span of outstanding numbers, not their count: a new transfer is
/// refused while its number would push the oldest outstanding transfer out
/// of the window, so draining the queue in order is what frees it.
///
/// # Loss protection
///
/// This sender emits each message exactly once and packs the queue in
/// arrival order, except that Transfer Cancels go ahead of every bundle (see
/// [`Self::cancel`]) and a transfer that cannot supply its next segment is
/// passed over (see [`Self::next_pdu`]).  The repetition (Section 6) the
/// protocol permits is not implemented here, so a lost PDU loses the
/// messages it carried; a bundle that fits one PDU is lost outright, and a
/// segmented one is lost when its transfer expires at the receiver.  Those
/// are properties of the link to weigh when choosing it.
///
/// # Link framing
///
/// [`SenderConfig::link_framing`] fixes the link's framing discipline at
/// construction.  By default the sender assumes fixed-size frames: every
/// PDU is padded to the configured [`PduSize`], and a bundle that fits in
/// one PDU is emitted as a type-2 Bundle Message (Section 8.1), whose 4-byte
/// header lets it share a PDU with another transfer's segments, be followed
/// by padding, and carry hints.  [`LinkFraming::Variable`] pads only up to
/// a floor, if one is set, and may additionally emit fitting bundles as
/// bare bundle frames for a peer that accepts them (see
/// [`BundleFraming::Bare`] for the padding caveat).
///
/// Every outbound unit, bare frames included, passes through the one
/// send queue: a bare frame is emitted in arrival order behind the
/// messages queued before it, counts against the [`SendQueueHighWatermark`], and is
/// visible to whatever schedules that queue.  Writing bare bundles to the
/// link around the sender would instead let them race and starve the
/// transfers queued here.
///
/// # Concurrency
///
/// `Sender` is single-owner: it is mutated through `&mut self`, and the
/// `tower` impls follow suit.  Under the `tower` feature,
/// `Service::poll_ready` parks the caller while the transfer window is
/// saturated or the send queue is at its [`SendQueueHighWatermark`]; the window gate
/// applies to unsegmented bundles too, since `poll_ready` cannot see the
/// request.  Every parked task is woken when `Stream::poll_next` drains a
/// PDU or [`Self::cancel`] frees a slot or queued bytes.  `poll_next`
/// parks while nothing is ready to pack and never yields `Ready(None)`.
/// Only the drain frees a full window or a queue at its high watermark, so run producers and the
/// drain from separate tasks, or from one task that selects over both; a
/// task that awaits `ready()` before polling the drain stops for good once
/// the window fills.
///
/// Several producers may share a `Sender` through `Arc<Mutex<_>>`.
/// Admission is not reserved between `poll_ready` and `call` (or between
/// [`Self::is_window_available`] and [`Self::enqueue`]), so a producer
/// takes the lock, polls, and if ready calls before releasing it; and it
/// releases the lock before parking, so the drain can take it.  A
/// `WindowFull` from `enqueue` after a positive check means another
/// producer got there first; check again.  The drain has one consumer.  Do
/// **not** use `tower::buffer::Buffer`: it moves the `Sender` into a worker
/// task and exposes only the `Service` half, so the drain and
/// [`Self::cancel`] become unreachable, PDUs never leave, and window slots
/// never free.
pub struct Sender {
    /// Stamped on every [`SendId`] this sender issues, so that an ID or
    /// handle from another sender is refused rather than taken for a
    /// bundle of this one's under the same local ID.
    owner: Owner,
    pdu_size: PduSize,
    /// Backpressure threshold on `queued_bytes`; enforced by the `tower`
    /// `Service::poll_ready` rather than by `enqueue` or `push` themselves.
    send_queue_high_watermark: SendQueueHighWatermark,
    /// The bundle bytes held in the queue and in `assembling`.  A `u64`, so
    /// that clones of one buffer queued many times cannot wrap it on a
    /// 32-bit target; `enqueue` and `push` refuse an addition that would
    /// wrap it, so every later subtraction has its bytes counted.
    queued_bytes: u64,
    /// Owns the set of outstanding transfer numbers and the Section 5 window
    /// rule.  [`Self::cancel`] and the End-packing release in
    /// [`Self::next_pdu`] only act on numbers it reports as outstanding.
    allocator: TransferNumberAllocator,
    link_framing: LinkFraming,
    segment_cut_strategy: SegmentCutStrategy,
    /// The queue in packing order: bundles that fit one PDU, and the
    /// segmented transfers that could supply a segment to some PDU.
    order: Order,
    /// The starved transfers (see [`QueuedTransfer::starved`]), set aside
    /// from `order` so that packing does not pass over them PDU after PDU.
    /// Each keeps its queue position, and returns to `order` when a push,
    /// or a flush's cut, leaves it able to supply.
    parked: BTreeMap<Seq, Box<QueuedTransfer>>,
    /// The queue position of every bundle in `order` and `parked`.
    index: BTreeMap<LocalId, Seq>,
    /// The position the next queued bundle takes.
    next_seq: Seq,
    /// The transfer numbers of the Transfer Cancels to send, oldest first,
    /// ahead of every bundle.
    cancels: VecDeque<u32>,
    /// Bundles that fit one PDU, begun and not yet fully pushed.  Each
    /// joins `order` when its last byte is pushed.
    assembling: BTreeMap<LocalId, Assembly>,
    /// The counter value for the next [`SendKind::Message`] or
    /// [`SendKind::Bare`]; wraps.
    next_bundle_id: u32,
    /// Every task parked in the `tower` `Service::poll_ready`, woken when a
    /// window slot frees or the send queue drains below its high watermark.  A list
    /// rather than a slot so that several producers sharing the sender
    /// through a mutex are all woken; re-registration by a task already
    /// present is deduplicated with `Waker::will_wake`.
    #[cfg(feature = "tower")]
    enqueue_wakers: Vec<Waker>,
    /// The task parked in `Stream::poll_next`, woken when a bundle joins
    /// `order` (through [`Self::queue`]), a Transfer Cancel is queued, or a
    /// push lets a queued transfer open a PDU.  Single-slot: the drain has
    /// one consumer.
    #[cfg(feature = "tower")]
    drain_waker: Option<Waker>,
    /// The queue entries examined by packing, summed over every PDU.
    #[cfg(test)]
    probes: usize,
}

/// Summarises the queue rather than printing it: the queued bundles'
/// bytes would make the output as large as the backlog.
impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Sender");
        d.field("pdu_size", &self.pdu_size)
            .field("send_queue_high_watermark", &self.send_queue_high_watermark)
            .field("link_framing", &self.link_framing)
            .field("segment_cut_strategy", &self.segment_cut_strategy)
            .field("window_size", &self.allocator.window_size())
            .field("transfers_outstanding", &self.allocator.in_progress())
            .field("window_available", &self.is_window_available())
            .field("queued", &(self.order.len() + self.parked.len()))
            .field("parked", &self.parked.len())
            .field("cancels", &self.cancels.len())
            .field("queued_bytes", &self.queued_bytes)
            .field("assembling", &self.assembling.len())
            .field("next_bundle_id", &self.next_bundle_id);
        #[cfg(feature = "tower")]
        d.field("enqueue_wakers", &self.enqueue_wakers.len())
            .field("drain_waker", &self.drain_waker.is_some());
        d.finish()
    }
}

impl Sender {
    /// Create a new sender that will allocate `initial_transfer_number` as
    /// its first transfer number.
    ///
    /// The BTP-U spec recommends choosing this value unpredictably (typically
    /// from a random source) to reduce the likelihood of a receiver mistaking
    /// the new sender for an old one after a restart; see `Sender::try_from_rng`
    /// and `Sender::from_rng` (under the `rand` feature) for the common case
    /// of seeding from an RNG.
    pub fn new(config: SenderConfig, initial_transfer_number: u32) -> Self {
        Self {
            owner: Owner::new(),
            pdu_size: config.pdu_size,
            send_queue_high_watermark: config.send_queue_high_watermark,
            queued_bytes: 0,
            allocator: TransferNumberAllocator::new(config.window_size, initial_transfer_number),
            link_framing: config.link_framing,
            segment_cut_strategy: config.segment_cut_strategy,
            order: Order::default(),
            parked: BTreeMap::new(),
            index: BTreeMap::new(),
            next_seq: Seq(0),
            cancels: VecDeque::new(),
            assembling: BTreeMap::new(),
            next_bundle_id: 0,
            #[cfg(feature = "tower")]
            enqueue_wakers: Vec::new(),
            #[cfg(feature = "tower")]
            drain_waker: None,
            #[cfg(test)]
            probes: 0,
        }
    }

    /// Create a new sender with the initial transfer number seeded from `rng`.
    /// Convenience wrapper over [`Self::new`].
    #[cfg(feature = "rand")]
    pub fn from_rng<R: Rng>(config: SenderConfig, rng: &mut R) -> Self {
        Self::new(config, rng.next_u32())
    }

    /// Create a new sender with the initial transfer number seeded from a
    /// fallible `rng`, such as the operating system's `rand::rngs::SysRng`.
    ///
    /// # Errors
    ///
    /// Returns the RNG's error if it cannot produce a value.
    #[cfg(feature = "rand")]
    pub fn try_from_rng<R: TryRng>(config: SenderConfig, rng: &mut R) -> Result<Self, R::Error> {
        Ok(Self::new(config, rng.try_next_u32()?))
    }

    /// Take the next queue position for the bundle `id`.
    fn take_seq(&mut self, id: LocalId) -> Seq {
        let seq = self.next_seq;
        self.next_seq = seq.next();
        self.index.insert(id, seq);
        seq
    }

    /// Append `entry` to the queue and wake the drain.
    fn queue(&mut self, entry: QueueEntry) {
        let seq = self.take_seq(entry.local_id());
        self.queue_at(seq, entry);
    }

    /// Append the transfer `t` to the queue, setting it aside if it is
    /// starved, and otherwise waking the drain.
    fn queue_transfer(&mut self, t: QueuedTransfer) {
        let seq = self.take_seq(LocalId::Transfer(t.transfer_number));
        let t = Box::new(t);
        if t.starved() {
            self.parked.insert(seq, t);
        } else {
            self.queue_at(seq, QueueEntry::Transfer(t));
        }
    }

    /// Put `entry` in `order` at `seq` and wake the drain.
    fn queue_at(&mut self, seq: Seq, entry: QueueEntry) {
        self.order.insert(seq, entry);
        self.wake_drain();
    }

    /// Queue a Transfer Cancel, behind those already queued and ahead of
    /// every bundle, and wake the drain.
    fn queue_cancel(&mut self, transfer_number: u32) {
        self.cancels.push_back(transfer_number);
        self.wake_drain();
    }

    /// Remove the entry at `seq` from `order`, uncounting the bytes it
    /// holds.
    fn dequeue(&mut self, seq: Seq) -> Option<QueueEntry> {
        let entry = self.order.remove(seq)?;
        self.index.remove(&entry.local_id());
        self.queued_bytes -= entry.queued_bytes();
        Some(entry)
    }

    /// Remove the transfer numbered `transfer_number` from the queue,
    /// parked or not, uncounting the bytes it holds.
    fn dequeue_transfer(&mut self, transfer_number: u32) -> Option<Box<QueuedTransfer>> {
        let seq = self.index.remove(&LocalId::Transfer(transfer_number))?;
        let t = match self.order.remove(seq) {
            Some(QueueEntry::Transfer(t)) => t,
            Some(_) => unreachable!("the index names a transfer by its number"),
            None => self.parked.remove(&seq)?,
        };
        self.queued_bytes -= t.buffered as u64;
        Some(t)
    }

    /// The transfer at `seq`, parked or not.
    fn transfer_mut(&mut self, seq: Seq) -> Option<&mut QueuedTransfer> {
        match self.order.get_mut(seq) {
            Some(entry) => entry.as_transfer_mut(),
            None => self.parked.get_mut(&seq).map(Box::as_mut),
        }
    }

    /// Move the transfer at `seq` between `order` and `parked` as
    /// [`QueuedTransfer::starved`] says, after a push or a cut.  Does not
    /// wake the drain.
    fn settle(&mut self, seq: Seq) {
        if let Some(QueueEntry::Transfer(t)) = self.order.get(seq)
            && t.starved()
        {
            let Some(QueueEntry::Transfer(t)) = self.order.remove(seq) else {
                unreachable!("checked above");
            };
            self.parked.insert(seq, t);
        } else if self.parked.get(&seq).is_some_and(|t| !t.starved()) {
            let t = self.parked.remove(&seq).expect("checked above");
            self.order.insert(seq, QueueEntry::Transfer(t));
        }
    }

    /// The transfer numbered `transfer_number`, parked or not.
    fn find_transfer(&self, transfer_number: u32) -> Option<&QueuedTransfer> {
        let seq = self.index.get(&LocalId::Transfer(transfer_number))?;
        match self.order.get(*seq) {
            Some(entry) => entry.as_transfer(),
            None => self.parked.get(seq).map(Box::as_ref),
        }
    }

    /// The queue in packing order, from `from`: `order` and `parked`
    /// merged, each entry with its position.  Transfer Cancels aside.
    fn queued_from(&self, from: Seq) -> impl Iterator<Item = (Seq, Queued<'_>)> + '_ {
        let mut order = self.order.range_from(from).peekable();
        let mut parked = self.parked.range(from..).peekable();
        from_fn(move || {
            let from_order = match (order.peek(), parked.peek()) {
                (Some((a, _)), Some((b, _))) => a < b,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if from_order {
                order.next().map(|(seq, e)| (seq, Queued::Entry(e)))
            } else {
                parked.next().map(|(&seq, t)| (seq, Queued::Parked(t)))
            }
        })
    }

    /// Wake every task parked on a `Service::poll_ready` that returned
    /// `Pending` because the window was full or the send queue was at its
    /// high watermark, once both have room: the same predicate `poll_ready` gates
    /// on, so a woken task finds the service ready.  No-op without the
    /// `tower` feature.
    #[cfg(feature = "tower")]
    fn wake_enqueue(&mut self) {
        if self.is_window_available() && !self.is_send_queue_high() {
            for w in self.enqueue_wakers.drain(..) {
                w.wake();
            }
        }
    }
    #[cfg(not(feature = "tower"))]
    fn wake_enqueue(&mut self) {}

    /// Wake any task parked on a `Stream::poll_next` that returned `Pending`
    /// because nothing was ready. No-op without the `tower` feature.
    #[cfg(feature = "tower")]
    fn wake_drain(&mut self) {
        if let Some(w) = self.drain_waker.take() {
            w.wake();
        }
    }
    #[cfg(not(feature = "tower"))]
    fn wake_drain(&mut self) {}

    /// Whether a segmented bundle could currently be admitted without
    /// violating the transfer window: whether its transfer number would keep
    /// the oldest outstanding transfer inside the window.
    ///
    /// The `tower` `Service::poll_ready` uses this as its window gate, so it
    /// is exactly the predicate [`Self::enqueue`] applies when segmenting.
    pub fn is_window_available(&self) -> bool {
        self.allocator.can_allocate()
    }

    /// Whether the bytes queued and not yet packed have reached the
    /// configured [`SendQueueHighWatermark`].
    ///
    /// The `tower` `Service::poll_ready` uses this as its admission gate:
    /// unsegmented bundles take no window slot, so without it the queue
    /// would grow without bound whenever the drain side is slower.  Direct
    /// [`Self::enqueue`] and [`Self::push`] callers can poll it to pace
    /// themselves the same way, draining [`Self::next_pdu`] when it reports
    /// full.
    ///
    /// Gate [`Self::begin`] and [`Self::enqueue`] on it, and each push of a
    /// bundle already begun on [`Self::is_push_ready`].  A segment goes out
    /// only once its bytes are pushed (see [`SegmentCutStrategy`]), so when
    /// every bundle in the queue is waiting on its producer, only pushes can
    /// free it; a producer that waits here mid-bundle can then wait on
    /// bytes only it would supply.
    pub fn is_send_queue_high(&self) -> bool {
        self.queued_bytes >= self.send_queue_high_watermark.get() as u64
    }

    /// Whether a producer pacing itself on the [`SendQueueHighWatermark`] should
    /// push the next chunk of the bundle `handle` was begun for now, rather
    /// than drain [`Self::next_pdu`] first.
    ///
    /// True while the send queue is below its high watermark, and otherwise
    /// while the bundle cannot open a PDU without more bytes: a segmented
    /// bundle whose next segment is waiting on its producer, though it may
    /// still fill the tail of a PDU another entry opened, or a bundle that
    /// fits one PDU and is not yet fully pushed.  A producer that waits only when
    /// this is false therefore never waits on bytes only it would supply.
    /// A bundle holds less than one segment (or one PDU) when a push past
    /// the watermark is admitted for it, so the queue exceeds its watermark
    /// by less than one segment and one chunk per bundle in progress, a
    /// chunk being whatever size the producer pushes.
    ///
    /// Also true for a handle whose bundle is not in progress, or that
    /// another sender issued, so that [`Self::push`] reports why.  Nothing
    /// wakes a producer when this turns true; it is checked again after
    /// draining.
    ///
    /// Costs a scan of the send queue for a segmented bundle while the
    /// queue is at its high watermark.
    pub fn is_push_ready(&self, handle: &SendHandle) -> bool {
        if !self.is_send_queue_high() {
            return true;
        }
        let LocalId::Transfer(transfer_number) = handle.id.local else {
            return true;
        };
        if handle.id.owner != self.owner {
            return true;
        }
        self.find_transfer(transfer_number)
            .is_none_or(|t| t.waiting(self.pdu_size.get()))
    }

    /// The bundle bytes queued and not yet packed into a PDU, as
    /// [`SendQueueHighWatermark`] counts them.
    pub fn queued_bytes(&self) -> u64 {
        self.queued_bytes
    }

    /// The queued byte count with `len` more bytes counted, or
    /// [`Error::QueuedBytesOverflow`] if it would wrap.
    fn counted(&self, len: usize) -> Result<u64> {
        self.queued_bytes
            .checked_add(len as u64)
            .ok_or(Error::QueuedBytesOverflow)
    }

    /// Register a task to be woken when a window slot frees or the send
    /// queue drains.  Used by the `tower` Service impl from `poll_ready`.
    /// Costs a scan of the tasks already parked, to skip one that is.
    ///
    /// Deduplication relies on `Waker::will_wake`.  An executor that polls
    /// a pending task again without waking it, with a waker that is not
    /// `will_wake`-equal to the last, would add one entry per such poll
    /// until the next wake clears the list; polling without a wake breaks
    /// the executor contract, and one task's wakers compare equal in the
    /// common executors, so the list stays one entry per producer.
    #[cfg(feature = "tower")]
    pub(crate) fn register_enqueue_waker(&mut self, waker: &Waker) {
        if !self.enqueue_wakers.iter().any(|w| w.will_wake(waker)) {
            self.enqueue_wakers.push(waker.clone());
        }
    }

    /// Register a waker to be notified when a new PDU becomes available.
    /// Used by the `tower` Stream impl from `poll_next`.
    #[cfg(feature = "tower")]
    pub(crate) fn register_drain_waker(&mut self, waker: Waker) {
        self.drain_waker = Some(waker);
    }

    /// Queue a bundle for transmission, returning the ID under which
    /// [`Pdu::carried`] will report it.
    ///
    /// If the bundle fits in a single PDU (as a Bundle message), it is emitted
    /// without segmentation and the ID is a [`SendKind::Message`].
    /// Otherwise, it is split into Transfer Segment and Transfer End
    /// messages under a newly allocated transfer number, and the ID is a
    /// [`SendKind::Transfer`].
    ///
    /// Under [`BundleFraming::Bare`], a bundle that fits in a PDU, carries no
    /// caller hints, and begins with a bundle-reserved byte is instead queued
    /// as a bare frame and the ID is a [`SendKind::Bare`].
    ///
    /// Caller hints from `options` ride on the Bundle message or the first
    /// segment (hints are transfer-scoped, Section 7.2); the sender derives
    /// and attaches the Bundle Length hint itself when segmenting.
    ///
    /// An empty `data` is rejected with [`Error::Empty`]: it cannot be
    /// a valid bundle (Section 8.1), and nothing is queued.  So is a
    /// bundle whose bytes would overflow [`Self::queued_bytes`]
    /// ([`Error::QueuedBytesOverflow`]).
    ///
    /// Segments are copied out of `data` into the PDUs that carry them,
    /// and `data` is released as its last segment is packed.
    pub fn enqueue(&mut self, data: Bytes, options: SendOptions) -> Result<SendId> {
        let bare_ok = frame_kind(&data) != FrameKind::BtpuPdu;
        let len = data.len();
        let queued_bytes = self.counted(len)?;
        let id = match self.plan(len, options, bare_ok)? {
            Planned::Whole { id, hints } => {
                self.queue_whole(id, hints, data);
                id
            }
            Planned::Transfer(mut t) => {
                t.push(data);
                let id = LocalId::Transfer(t.transfer_number);
                self.queue_transfer(t);
                id
            }
        };
        self.queued_bytes = queued_bytes;
        Ok(SendId::new(self.owner, id))
    }

    /// Begin a bundle of `total_len` bytes whose bytes are pushed later,
    /// returning the handle they are pushed through.
    ///
    /// The bundle is framed as [`Self::enqueue`] would frame `total_len`
    /// bytes with these `options`, and is refused for the same reasons
    /// before anything is queued; a segmented bundle takes its window slot
    /// here.  Under [`BundleFraming::Bare`], `begin` cannot see the first
    /// byte, so a fitting bundle without caller hints is begun as a
    /// [`SendKind::Bare`] and its first chunk must start with a
    /// bundle-reserved byte (see [`Error::NotABundle`]).
    ///
    /// Push the bytes with [`Self::push`] and end the bundle with
    /// [`Self::finish`], or abandon it with [`Self::cancel`].  A bundle that
    /// fits one PDU joins the queue when its last byte is pushed, so such
    /// bundles go out in the order they are completed.  A segmented bundle
    /// is queued here, and each segment goes out once its bytes are pushed
    /// (see [`SegmentCutStrategy`]); until then the bundles queued behind it
    /// go ahead of it.
    pub fn begin(&mut self, total_len: usize, options: SendOptions) -> Result<SendHandle> {
        let id = match self.plan(total_len, options, true)? {
            Planned::Whole { id, hints } => {
                self.assembling.insert(
                    id,
                    Assembly {
                        hints,
                        chunks: SmallVec::new(),
                        len: 0,
                    },
                );
                id
            }
            Planned::Transfer(t) => {
                let id = LocalId::Transfer(t.transfer_number);
                self.queue_transfer(t);
                id
            }
        };
        Ok(SendHandle {
            id: SendId::new(self.owner, id),
            total_len,
            pushed: 0,
        })
    }

    /// Push the next bytes of the bundle `handle` was begun for.
    ///
    /// The chunk is held, not copied, until the PDUs carrying it are
    /// packed.  An empty chunk does nothing.
    ///
    /// # Errors
    ///
    /// Nothing is pushed if the chunk would take the bundle past its
    /// length ([`Error::Overrun`]), the bundle has been cancelled
    /// ([`Error::NotInProgress`]), the first chunk of a bare bundle frame is
    /// not a bundle ([`Error::NotABundle`]), `handle` is another sender's
    /// ([`Error::ForeignHandle`]), or the chunk's bytes would overflow
    /// [`Self::queued_bytes`] ([`Error::QueuedBytesOverflow`]).  The bundle stays as it was in each case.
    pub fn push(&mut self, handle: &mut SendHandle, chunk: Bytes) -> Result<()> {
        if handle.id.owner != self.owner {
            return Err(Error::ForeignHandle);
        }
        if chunk.is_empty() {
            return Ok(());
        }
        let len = chunk.len();
        let left = handle.total_len - handle.pushed;
        if len > left {
            return Err(Error::Overrun {
                total_len: handle.total_len,
                pushed: handle.pushed,
                chunk: len,
            });
        }
        let queued_bytes = self.counted(len)?;

        let local = handle.id.local;
        if let LocalId::Transfer(_) = local {
            let seq = *self.index.get(&local).ok_or(Error::NotInProgress)?;
            let pdu_size = self.pdu_size.get();
            let transfer = self
                .transfer_mut(seq)
                .expect("the index names queued transfers");
            transfer.push(chunk);
            let waiting = transfer.waiting(pdu_size);
            self.settle(seq);
            // The drain parks only when nothing can open a PDU, so a push
            // that leaves the transfer waiting gives it nothing to do.
            if !waiting {
                self.wake_drain();
            }
        } else {
            let assembly = self
                .assembling
                .get_mut(&local)
                .ok_or(Error::NotInProgress)?;
            if assembly.len == 0
                && matches!(local, LocalId::Bare(_))
                && frame_kind(&chunk) == FrameKind::BtpuPdu
            {
                return Err(Error::NotABundle);
            }
            assembly.chunks.push(chunk);
            assembly.len += len;
            if len == left {
                let Assembly { hints, chunks, .. } =
                    self.assembling.remove(&local).expect("found above");
                self.queue_whole(local, hints, gather(chunks, handle.total_len));
            }
        }
        self.queued_bytes = queued_bytes;
        handle.pushed += len;
        Ok(())
    }

    /// End the bundle `handle` was begun for, returning its ID.
    ///
    /// The bundle's last bytes may still be queued; [`Pdu::carried`] reports
    /// when they leave.
    ///
    /// # Errors
    ///
    /// [`Error::Underrun`] if fewer than `total_len` bytes were pushed.  The
    /// bundle is then cancelled, as by [`Self::cancel`], since its missing
    /// bytes can never arrive.  [`Error::ForeignHandle`] if `handle` is
    /// another sender's; nothing is changed.
    pub fn finish(&mut self, handle: SendHandle) -> Result<SendId> {
        if handle.id.owner != self.owner {
            return Err(Error::ForeignHandle);
        }
        if handle.pushed < handle.total_len {
            let error = Error::Underrun {
                total_len: handle.total_len,
                pushed: handle.pushed,
            };
            self.cancel(handle);
            return Err(error);
        }
        Ok(handle.id)
    }

    /// Decide how a bundle of `total_len` bytes is framed, taking its ID
    /// and, if it is segmented, its window slot.  `bare_ok` is whether its
    /// first byte allows a bare bundle frame.
    fn plan(&mut self, total_len: usize, options: SendOptions, bare_ok: bool) -> Result<Planned> {
        if total_len == 0 {
            return Err(Error::Empty);
        }
        let pdu_size = self.pdu_size.get();

        // The sender owns the Bundle Length hint, so a caller-supplied one
        // is discarded.
        let mut hints = options.hints;
        hints.remove(HintType::BUNDLE_LENGTH);

        if self.bare_bundles()
            && hints.is_empty()
            && (self.padded_len()..=pdu_size).contains(&total_len)
            && bare_ok
        {
            // A bare frame needs no header, so it may use the whole PDU.
            // One shorter than the floor cannot be padded, since padding
            // after it would read as bundle bytes, so it goes out as a
            // Bundle Message instead.
            return Ok(Planned::Whole {
                id: LocalId::Bare(self.take_bundle_id()),
                hints: Vec::new(),
            });
        }

        if total_len <= pdu_size.saturating_sub(HEADER_SIZE + hints.encoded_len()) {
            // Fits in a single Bundle message.
            return Ok(Planned::Whole {
                id: LocalId::Message(self.take_bundle_id()),
                hints: hints.into_vec(),
            });
        }

        // Segment the bundle.  Size the segments before taking a transfer
        // number so a PDU too small to carry them never touches the window.
        let capacity = pdu_size.saturating_sub(SEGMENT_FRAMING);
        hints.insert(HintItem::BundleLength(total_len as u64));
        let first_segment_framing = SEGMENT_FRAMING + hints.encoded_len();
        let first_capacity = pdu_size.saturating_sub(first_segment_framing);

        if capacity == 0 || first_capacity == 0 {
            return Err(Error::PduTooSmall {
                required: first_segment_framing + 1,
                pdu_size,
            });
        }
        // Every segment between the first and the last carries at least
        // half of `capacity` (see `QueuedTransfer::chunk_for`), and the
        // last index must fit the 32-bit field (Section 8.2) short of
        // `u32::MAX`, which the receiver treats as a count of segments it
        // can never hold.
        let last_index = (total_len / capacity.div_ceil(2)).checked_add(1);
        if last_index
            .and_then(|i| u32::try_from(i).ok())
            .is_none_or(|i| i == u32::MAX)
        {
            return Err(Error::TooManySegments {
                len: total_len,
                pdu_size,
            });
        }

        let transfer_number = self.allocator.allocate()?;
        Ok(Planned::Transfer(QueuedTransfer::new(
            transfer_number,
            total_len,
            hints.into_vec(),
            first_capacity,
            capacity,
            self.segment_cut_strategy,
        )))
    }

    /// Queue a whole bundle as `plan` framed it.  The caller counts its
    /// bytes.
    fn queue_whole(&mut self, id: LocalId, hints: Vec<HintItem>, data: Bytes) {
        let entry = match id {
            LocalId::Bare(id) => QueueEntry::BareBundle { id, data },
            _ => self.message_entry(id, Message::Bundle { hints, data }),
        };
        self.queue(entry);
    }

    /// Abandon a bundle the sender has not finished emitting.
    ///
    /// For a [`SendKind::Transfer`], the transfer's window slot is
    /// freed and its segments not yet packed are discarded.  If any had
    /// already been emitted, a Transfer Cancel message is queued ahead of
    /// every bundle waiting, behind only the Cancels queued before it, so
    /// the receiver discards what it holds (Section 4.2) as soon as the
    /// next PDU arrives; if none had, the receiver never learned of the
    /// transfer and no Cancel is sent.
    ///
    /// A [`SendKind::Message`] or [`SendKind::Bare`] still
    /// in the queue is removed from it.  Such a bundle travels whole, so the
    /// receiver has seen none of it and nothing is sent in its place.
    ///
    /// A bundle begun with [`Self::begin`] may be cancelled at any point,
    /// by its handle or its ID, and the chunks pushed for it are dropped.
    ///
    /// Returns whether the bundle was cancelled.  Returns `false`, changing
    /// nothing, if the sender has nothing left to emit for `id`: it was
    /// never issued, was already cancelled, or its last bytes have been
    /// packed, in which case a PDU has listed it with
    /// [`Carried::completes`] set.  An ID from another sender is a bug in
    /// the caller: a debug build panics on it, and a release build returns
    /// `false`.
    ///
    /// Costs a scan of the send queue, and for a transfer a scan of the
    /// outstanding transfer numbers as well (at most the window size).
    pub fn cancel(&mut self, id: impl Into<SendId>) -> bool {
        let id = id.into();
        if !self.issued(id) {
            return false;
        }
        let cancelled = match id.local {
            LocalId::Transfer(transfer_number) => self.cancel_transfer(transfer_number),
            LocalId::Message(_) | LocalId::Bare(_) => match self.find_unsegmented(id.local) {
                Some(Unsegmented::Queued(seq)) => {
                    self.dequeue(seq);
                    true
                }
                Some(Unsegmented::Assembling) => {
                    let assembly = self.assembling.remove(&id.local).expect("found above");
                    self.queued_bytes -= assembly.len as u64;
                    true
                }
                None => false,
            },
        };
        if cancelled {
            // A window slot or queued bytes freed.  A Transfer Cancel
            // queued in its place has woken the drain already.
            self.wake_enqueue();
        }
        cancelled
    }

    /// The [`SendKind::Transfer`] case of [`Self::cancel`].
    fn cancel_transfer(&mut self, transfer_number: u32) -> bool {
        if !self.allocator.release(transfer_number) {
            return false;
        }

        // An outstanding transfer is one queue entry until its End is
        // packed.  Segments are cut in index order, so a transfer that has
        // not started means nothing of it has been emitted.
        let nothing_emitted = self
            .dequeue_transfer(transfer_number)
            .is_some_and(|t| !t.started());
        if !nothing_emitted {
            // Ahead of every bundle, so the receiver can drop what it holds
            // without waiting out the backlog.  Emitting a smaller number
            // early cannot raise the greatest emitted, so Section 5 still
            // holds.
            self.queue_cancel(transfer_number);
        }
        true
    }
    /// Returns `true` if there are messages pending for transmission.
    pub fn has_pending(&self) -> bool {
        !(self.order.is_empty() && self.parked.is_empty() && self.cancels.is_empty())
    }

    /// Whether the sender still has bytes of the bundle `id` names to
    /// emit: what [`Self::cancel`] would act on.  A transfer is outstanding,
    /// holding its window slot, until its End is packed or it is cancelled;
    /// a Bundle Message or bare frame until it is packed or cancelled.  An
    /// ID from another sender is a bug in the caller: a debug build panics
    /// on it, and a release build returns `false`.
    ///
    /// Costs a scan of the outstanding transfer numbers (at most the window
    /// size) for a transfer, and of the send queue otherwise.
    pub fn is_outstanding(&self, id: SendId) -> bool {
        if !self.issued(id) {
            return false;
        }
        match id.local {
            LocalId::Transfer(transfer_number) => self.allocator.is_outstanding(transfer_number),
            LocalId::Message(_) | LocalId::Bare(_) => self.find_unsegmented(id.local).is_some(),
        }
    }

    /// The IDs of every bundle [`Self::is_outstanding`] would report: the
    /// queued bundles in queue order, then those begun and not yet fully
    /// pushed.  A CLA that has lost a [`SendHandle`] can find its bundle
    /// here and [`Self::cancel`] it.
    ///
    /// A full pass costs a walk of the send queue.
    pub fn outstanding(&self) -> impl Iterator<Item = SendId> + '_ {
        let owner = self.owner;
        self.queued_from(Seq(0))
            .map(|(_, queued)| queued.local_id())
            .chain(self.assembling.keys().copied())
            .map(move |local| SendId::new(owner, local))
    }

    /// Whether this sender issued `id`.  An ID from another sender is a bug
    /// in the caller, which a debug build stops at with a panic; a release
    /// build treats the ID as naming nothing.
    fn issued(&self, id: SendId) -> bool {
        let issued = id.owner == self.owner;
        debug_assert!(issued, "{id:?} was issued by another Sender");
        issued
    }

    /// Where the unsegmented bundle `id` is held, if it is.
    fn find_unsegmented(&self, id: LocalId) -> Option<Unsegmented> {
        if let Some(&seq) = self.index.get(&id) {
            return Some(Unsegmented::Queued(seq));
        }
        self.assembling
            .contains_key(&id)
            .then_some(Unsegmented::Assembling)
    }

    /// Wrap a Bundle Message for the queue, checking that it fits an empty
    /// PDU (see [`Self::pack`]).
    fn message_entry(&self, id: LocalId, message: Message) -> QueueEntry {
        debug_assert!(
            encoded_message_len(&message) <= self.pdu_size.get(),
            "message larger than the PDU"
        );
        QueueEntry::Message { id, message }
    }

    /// Take the next [`SendKind::Message`] or
    /// [`SendKind::Bare`] counter value.
    fn take_bundle_id(&mut self) -> u32 {
        let id = self.next_bundle_id;
        self.next_bundle_id = id.wrapping_add(1);
        id
    }

    /// Set the next unsegmented bundle ID, so a test can reach the wrap
    /// without queueing 2³² bundles.
    #[cfg(test)]
    fn set_next_bundle_id(&mut self, id: u32) {
        self.next_bundle_id = id;
    }

    /// The length every PDU is padded up to.
    fn padded_len(&self) -> usize {
        let pdu_size = self.pdu_size.get();
        match self.link_framing {
            LinkFraming::FixedSize => pdu_size,
            LinkFraming::Variable { min_pdu_len, .. } => min_pdu_len.min(pdu_size),
        }
    }

    /// Whether fitting bundles may be emitted as bare bundle frames.
    fn bare_bundles(&self) -> bool {
        matches!(
            self.link_framing,
            LinkFraming::Variable {
                bundle_framing: BundleFraming::Bare,
                ..
            }
        )
    }
}

/// How [`Sender::plan`] frames a bundle.
enum Planned {
    /// A Bundle Message or bare bundle frame, by the ID's variant.
    Whole { id: LocalId, hints: Vec<HintItem> },
    /// A segmented bundle, its window slot taken.
    Transfer(QueuedTransfer),
}

/// The pushed chunks of a whole bundle as one buffer, `len` bytes long.
fn gather(mut chunks: SmallVec<[Bytes; 1]>, len: usize) -> Bytes {
    if chunks.len() == 1 {
        return chunks.pop().expect("one chunk");
    }
    let mut data = BytesMut::with_capacity(len);
    for chunk in &chunks {
        data.extend_from_slice(chunk);
    }
    data.freeze()
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use bytes::Bytes;

    use super::*;
    use crate::codec::hint::{HintValue, Hints};

    #[test]
    fn unsegmented_ids_wrap_across_message_and_bare() {
        let mut s = Sender::new(
            SenderConfig {
                link_framing: LinkFraming::variable(BundleFraming::Bare),
                ..SenderConfig::default()
            },
            0,
        );
        s.set_next_bundle_id(u32::MAX);
        // 0x9F, a bundle's CBOR array head, lets a hint-free bundle go out
        // as a bare frame; the hinted one must be a Bundle Message.
        let bare = Bytes::from_static(&[0x9F, 0xFF]);
        let hinted = SendOptions {
            hints: Hints::from_iter([HintItem::Unknown {
                hint_type: HintType::new(0x40).unwrap(),
                value: HintValue::new(Bytes::from_static(&[1])).unwrap(),
            }]),
        };
        assert_eq!(
            s.enqueue(bare.clone(), SendOptions::default()),
            Ok(SendId::new(s.owner, LocalId::Bare(u32::MAX)))
        );
        assert_eq!(
            s.enqueue(bare.clone(), hinted),
            Ok(SendId::new(s.owner, LocalId::Message(0)))
        );
        assert_eq!(
            s.enqueue(bare, SendOptions::default()),
            Ok(SendId::new(s.owner, LocalId::Bare(1)))
        );
    }

    #[test]
    fn an_addition_that_would_overflow_the_queued_bytes_changes_nothing() {
        let mut s = Sender::new(
            SenderConfig {
                pdu_size: PduSize::new(64).unwrap(),
                ..SenderConfig::default()
            },
            0,
        );
        // Only clones of one buffer queued billions of times come near
        // `u64::MAX`, so the count is set there directly.
        s.queued_bytes = u64::MAX - 10;
        let over = Bytes::from(vec![0; 11]);
        assert_eq!(
            s.enqueue(over.clone(), SendOptions::default()),
            Err(Error::QueuedBytesOverflow)
        );
        assert_eq!(
            s.enqueue(Bytes::from(vec![0; 200]), SendOptions::default()),
            Err(Error::QueuedBytesOverflow)
        );
        assert!(!s.has_pending());

        let mut fitting = s.begin(11, SendOptions::default()).unwrap();
        let mut segmented = s.begin(200, SendOptions::default()).unwrap();
        // The refused segmented enqueue took no transfer number.
        assert_eq!(segmented.id(), SendId::new(s.owner, LocalId::Transfer(0)));
        for handle in [&mut fitting, &mut segmented] {
            assert_eq!(
                s.push(handle, over.clone()),
                Err(Error::QueuedBytesOverflow)
            );
            assert_eq!(handle.pushed(), 0);
        }
        assert_eq!(s.queued_bytes(), u64::MAX - 10);

        // Up to `u64::MAX` itself is counted.
        s.push(&mut segmented, over.slice(..10)).unwrap();
        assert_eq!(s.queued_bytes(), u64::MAX);
    }
}
