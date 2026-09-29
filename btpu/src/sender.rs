//! The sending end: queues bundles, allocates their transfer numbers within
//! the Section 5 window, and packs their messages into PDUs.

use alloc::{collections::VecDeque, vec::Vec};
#[cfg(feature = "tower")]
use core::task::Waker;
use core::{fmt, mem::take, num::NonZeroUsize, ops::Deref, slice};

use bytes::{Bytes, BytesMut};
#[cfg(feature = "rand")]
use rand_core::{Rng, TryRng};
use smallvec::SmallVec;

use crate::{
    codec::{
        encode_message, encoded_message_len,
        header::{HEADER_SIZE, MAX_CONTENT_LENGTH},
        hint::{HintItem, HintType, Hints},
        message::{FrameKind, Message, SEGMENT_FRAMING, TransferSegmentMessage, frame_kind},
        pad_pdu, segment_message_len,
    },
    // Aliased: this module has its own `Error`.
    transfer::{Error as TransferError, TransferNumberAllocator, WindowSize},
};

/// Shorthand for results whose error is [`enum@Error`] unless stated.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors from enqueuing bundles for transmission.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The bundle is empty.  A Bundle Message's content MUST be a valid
    /// bundle (Section 8.1), which an empty payload can never be; rejecting
    /// it here keeps the sender from emitting the zero-content Bundle
    /// Message the draft says SHOULD NOT be used.
    #[error("Empty data")]
    Empty,

    /// No transfer window slot is available.
    #[error(transparent)]
    Window(#[from] TransferError),

    /// The configured PDU is too small to carry even one byte of a segment
    /// of this bundle alongside its headers and hints.  The likeliest cause
    /// is a large caller-supplied hint chain.
    ///
    /// The first segment always carries the derived Bundle Length hint,
    /// whose value takes 1, 2, 4, or 8 bytes by the bundle's length, so
    /// with no caller hints the least PDU that can segment a bundle is 16,
    /// 17, 19, or 23 bytes, for bundles of up to 255 bytes, 64 KiB, 4 GiB,
    /// and beyond.
    #[error(
        "PDU size {pdu_size} cannot carry a segment: {required} bytes needed for its framing and one data byte"
    )]
    PduTooSmall {
        /// The least PDU size that would carry the first segment: its
        /// framing and hints, plus one data byte.
        required: usize,
        /// The configured PDU size.
        pdu_size: usize,
    },

    /// The bundle would need more segments than the 32-bit segment index
    /// can number at this PDU size (Section 8.2).
    ///
    /// Theoretical in practice: a bundle over 4 GiB needs a PDU of at least
    /// 23 bytes to segment (see [`Self::PduTooSmall`]), whose later
    /// segments carry at least 11 bytes each, so only a bundle of more than
    /// 44 GiB in memory can reach it, and none can on a 32-bit target.
    #[error("Bundle of {len} bytes needs more than 2^32 segments at PDU size {pdu_size}")]
    TooManySegments {
        /// The bundle's length in bytes.
        len: usize,
        /// The configured PDU size.
        pdu_size: usize,
    },
}

/// A validated convergence layer PDU size in bytes
/// ([`PduSize::MIN`]..=[`PduSize::MAX`]).
///
/// Construct via [`PduSize::new`] or [`TryFrom<usize>`], which enforce the
/// bounds at the edge; every consumer of a `PduSize` can then rely on them.
/// Together they guarantee that anything [`Sender::enqueue`] accepts can
/// eventually be drained by [`Sender::next_pdu`]: no queued message is ever
/// larger than a PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct PduSize(NonZeroUsize);

impl PduSize {
    /// What the value configures, as error messages name it.
    const NAME: &str = "PDU size";

    /// Minimum supported convergence layer PDU size: one message header.
    ///
    /// Below this, `enqueue` could accept a message (the header alone is
    /// [`HEADER_SIZE`] bytes) that no PDU can ever carry, and `next_pdu`
    /// would emit pure padding forever without draining it.
    ///
    /// The minimum only keeps the sender live; a usable PDU is larger (see
    /// [`Error::PduTooSmall`] for what segmenting needs).
    pub const MIN: Self = Self(NonZeroUsize::new(HEADER_SIZE).unwrap());

    /// Maximum supported convergence layer PDU size.
    ///
    /// A PDU of this size can be exactly filled by a single message carrying
    /// the maximum 20-bit content length.  Any message or padding content the
    /// sender derives from a `PduSize` is guaranteed encodable; beyond this
    /// bound, segment capacities and padding lengths would overflow the
    /// 20-bit length field.
    pub const MAX: Self = Self(NonZeroUsize::new(HEADER_SIZE + MAX_CONTENT_LENGTH).unwrap());

    /// A common Ethernet-MTU-sized PDU (1500 bytes).
    pub const DEFAULT: Self = Self(NonZeroUsize::new(1500).unwrap());

    /// Returns the PDU size for `bytes`, or `None` if it is outside
    /// [`MIN`](Self::MIN)..=[`MAX`](Self::MAX).
    pub const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(n) if bytes >= Self::MIN.get() && bytes <= Self::MAX.get() => Some(Self(n)),
            _ => None,
        }
    }

    /// Returns the PDU size as a plain integer.
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for PduSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

config_newtype!(PduSize: usize);

/// A validated admission bound on the sender's pending queue, in queue
/// entries (non-zero).
///
/// An entry is one unit of the queue: a Bundle Message, a bare bundle
/// frame, a Transfer Cancel, or a whole segmented transfer, whose segments
/// are cut from the enqueued buffer one at a time as PDUs are packed rather
/// than queued individually.  The depth is an admission gate: a bundle is
/// admitted while the queue holds fewer than `depth` entries, so a caller
/// that respects the gate queues at most `depth` bundles' worth of data,
/// each shared with the caller's buffer rather than copied.
///
/// The bound drives backpressure, not errors: the `tower` `Service::poll_ready`
/// returns `Pending` while the queue is at depth, and direct
/// [`Sender::enqueue`] callers pace themselves with
/// [`Sender::is_send_queue_full`] and by draining [`Sender::next_pdu`];
/// `enqueue` itself never refuses on depth.  Construct via
/// [`SendQueueDepth::new`], [`TryFrom<usize>`], or [`From<NonZeroUsize>`];
/// a zero depth would park `poll_ready` forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct SendQueueDepth(NonZeroUsize);

impl SendQueueDepth {
    /// What the value configures, as error messages name it.
    const NAME: &str = "send queue depth";

    /// The smallest depth, one entry.
    pub const MIN: Self = Self(NonZeroUsize::MIN);

    /// The largest depth, `usize::MAX` entries.
    pub const MAX: Self = Self(NonZeroUsize::MAX);

    /// 64 entries.
    pub const DEFAULT: Self = Self(NonZeroUsize::new(64).unwrap());

    /// Returns the depth for `entries`, or `None` if it is zero.
    pub const fn new(entries: usize) -> Option<Self> {
        match NonZeroUsize::new(entries) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Returns the depth as a plain integer.
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for SendQueueDepth {
    fn default() -> Self {
        Self::DEFAULT
    }
}

config_newtype!(SendQueueDepth: usize, NonZeroUsize);

/// The framing discipline of the link a [`Sender`] feeds.
///
/// It decides whether [`Sender::next_pdu`] pads every PDU out to the
/// configured [`PduSize`] and whether a fitting bundle may travel without a
/// BTP-U header.  The combination "fixed-size frames with bare bundles" is
/// unrepresentable: a bare frame is the bundle's own bytes, so it cannot be
/// padded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")
)]
pub enum LinkFraming {
    /// Fixed-size frames (for example CCSDS frames): every PDU is padded to
    /// exactly the configured [`PduSize`] and a fitting bundle is always a
    /// Bundle Message.
    #[default]
    FixedSize,
    /// Variable-length PDUs (for example UDP datagrams): the [`PduSize`] is
    /// a ceiling, nothing is padded, and `bundle_framing` chooses how a
    /// fitting bundle is put on the wire.
    Variable {
        /// How a bundle that fits in one PDU is framed.  Default: a Bundle
        /// Message, so a configuration file may say `variable: {}`.
        #[cfg_attr(feature = "serde", serde(default))]
        bundle_framing: BundleFraming,
    },
}

/// How a [`Sender`] on a variable-length link frames a bundle that fits in
/// one PDU (see [`LinkFraming::Variable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "kebab-case")
)]
pub enum BundleFraming {
    /// A type-2 Bundle Message (Section 8.1): the 4-byte header is the only
    /// overhead, and it lets the bundle share a PDU with other messages,
    /// carry hints, and tolerate link padding after it (zero-fill decodes
    /// as Indefinite Padding).
    #[default]
    Message,
    /// The bundle's own bytes, with no BTP-U header, for a peer that accepts
    /// bare bundle frames.  Such a frame cannot be packed with other
    /// messages or carry hints, so it occupies a PDU of its own, and a
    /// bundle enqueued with hints is still sent as a Bundle Message.
    ///
    /// A receiver tells a bare frame from a PDU by its first byte, which the
    /// bundle-reserved message-type values (Section 12.1) guarantee; the
    /// sender therefore only emits bare frames whose first byte
    /// [`frame_kind`] classifies as a bundle, and frames anything else as a
    /// Bundle Message.
    ///
    /// **Padding links.** A bare frame carries nothing that tells the
    /// receiver where the bundle ends, so it is only suitable for a link
    /// that delivers the frame at exactly the length it was sent.  A link
    /// that pads frames to a minimum or fixed size (Ethernet's 46-octet
    /// minimum payload, for instance) delivers the padding as bundle bytes
    /// unless the peer can delimit the bundle itself (see
    /// [`BundleExtent`](crate::codec::BundleExtent) for this crate's receive
    /// side).  When in doubt, use [`BundleFraming::Message`].
    Bare,
}

/// Configuration for a [`Sender`].  Every field has a default, so a
/// configuration file may set only what it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(default, rename_all = "kebab-case")
)]
pub struct SenderConfig {
    /// The link's PDU size.  Default: 1500.
    pub pdu_size: PduSize,
    /// The Section 5 transfer window.  Default: 16.
    pub window_size: WindowSize,
    /// The pending-queue admission bound, in entries.  Default: 64.
    pub send_queue_depth: SendQueueDepth,
    /// The link's framing discipline.  Default: fixed-size frames.
    pub link_framing: LinkFraming,
}

/// Options for [`Sender::enqueue`].
///
/// `Default`-constructible; `SendOptions::default()` sends with no
/// caller-supplied hints.
// Not `#[non_exhaustive]`: `hints` carries anything the sender need not
// understand, so a new field means a new sender capability, and the
// compile break on callers' struct literals is the useful checklist.
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    /// Hint items to attach to the transfer, carried on the Bundle message
    /// (unsegmented) or the first segment (segmented) alongside the
    /// sender-derived Bundle Length hint, in ascending hint-type order.  A
    /// caller-supplied item of type [`HintType::BUNDLE_LENGTH`] is
    /// discarded: the sender derives the truthful value itself.
    pub hints: Hints,
}

/// A bundle plus its [`SendOptions`], the request type of the `tower`
/// `Service` impl.  `From<Bytes>` builds one with default options.
#[derive(Debug, Clone)]
pub struct SendRequest {
    /// The bundle to send.
    pub data: Bytes,
    /// How to send it.
    pub options: SendOptions,
}

impl From<Bytes> for SendRequest {
    fn from(data: Bytes) -> Self {
        Self {
            data,
            options: SendOptions::default(),
        }
    }
}

/// Identifies a bundle from [`Sender::enqueue`] until its last bytes leave
/// in a PDU, naming it in the [`Carried`] entries of every PDU that carries
/// part of it.
///
/// The variant records how the bundle travels.  A segmented bundle's ID is
/// its transfer number, which is outstanding in the window until its End is
/// packed or it is cancelled, so no two queued transfers share one.  Bundle
/// Messages and bare frames draw from one separate `u32` counter that
/// advances with every such enqueue and wraps at `u32::MAX`; their IDs are
/// unique while queued unless 2³² unsegmented bundles are queued at once.
///
/// Any ID may be passed back to [`Sender::cancel`] while the sender still
/// has bytes of the bundle to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SendId {
    /// Segmented under this transfer number.  The window slot is released
    /// by the sender itself once the transfer's End message has been packed
    /// into a PDU.
    Transfer(u32),
    /// Sent as a single Bundle Message.
    Message(u32),
    /// Sent as a bare bundle frame in a PDU of its own (see
    /// [`BundleFraming::Bare`]).
    Bare(u32),
}

// The tag fits the padding beside the `u32`, so an ID costs what a `u64`
// would; `Carried` lists rely on that staying true.
const _: () = assert!(size_of::<SendId>() == 8);

/// A bundle with bytes in a PDU, as listed by [`Pdu::carried`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Carried {
    /// The bundle, as [`Sender::enqueue`] reported it.
    pub id: SendId,
    /// Whether this PDU carries the bundle's last bytes: always for a
    /// Bundle Message or bare frame, and for a segmented bundle when the
    /// PDU holds its Transfer End.  Once the PDU holding it is written,
    /// the sender will emit nothing further for the bundle.
    pub completes: bool,
}

/// The [`Carried`] entries of one PDU, held inline up to
/// [`Self::INLINE`] entries and on the heap beyond that.
///
/// Dereferences to `[Carried]`.  Equality, hashing and `Debug` follow the
/// entries, not where they are stored.
///
/// A list keeps its heap buffer once it has one: when refilled by
/// [`Sender::next_pdu_into`] it writes into that buffer even for a PDU
/// whose entries would fit inline, so a reused list stops allocating once
/// it has grown to the largest PDU seen.  [`Self::with_capacity`] starts
/// with a heap buffer of a chosen size.
///
/// # Sizing
///
/// A PDU of valid BPv7 bundles carries at most `pdu_size / 32 + 1`
/// entries.  The smallest valid bundle is 28 bytes (RFC 9171: a 20-byte
/// primary block with its mandatory CRC and every EID `dtn:none`, an empty
/// payload block, and the indefinite-length array around them), so each
/// Bundle Message is at least 32 bytes with its header.  A transfer's first
/// segment fills its PDU, so at most one transfer shares a PDU with other
/// entries, by its Transfer End.  The sender does not parse bundles; one
/// enqueued below 28 bytes can exceed the bound, at the cost of the list
/// growing.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CarriedList(SmallVec<[Carried; CarriedList::INLINE]>);

impl CarriedList {
    /// Entries held without allocating.  A PDU lists one entry for its
    /// transfer's segments and one for each Bundle Message after a
    /// Transfer End, so traffic of bundles longer than a quarter of the PDU
    /// stays inline.
    pub const INLINE: usize = 4;

    /// An empty list, held inline.
    pub const fn new() -> Self {
        Self(SmallVec::new_const())
    }

    /// An empty list that holds `capacity` entries without allocating
    /// again: inline up to [`Self::INLINE`], otherwise in a heap buffer
    /// allocated now and kept for the list's life.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(SmallVec::with_capacity(capacity))
    }

    /// Entries the list holds without allocating.
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    /// Empty the list, keeping any heap buffer.
    fn clear(&mut self) {
        self.0.clear();
    }

    /// Append `item`, moving the entries to the heap if the inline slots
    /// are full.
    fn push(&mut self, item: Carried) {
        self.0.push(item);
    }

    fn last_mut(&mut self) -> Option<&mut Carried> {
        self.0.last_mut()
    }
}

impl Default for CarriedList {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for CarriedList {
    type Target = [Carried];

    fn deref(&self) -> &[Carried] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a CarriedList {
    type Item = &'a Carried;
    type IntoIter = slice::Iter<'a, Carried>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for CarriedList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A packed PDU and the bundles it carries, returned by [`Sender::next_pdu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    /// The PDU, ready for the link.
    pub data: Bytes,
    /// Every bundle with bytes in `data`, once each, in the order packed.
    /// Empty for a PDU holding only a Transfer Cancel.
    pub carried: CarriedList,
}

/// One unit of the pending send queue.  No `Debug`: it holds bundle bytes.
enum QueueEntry {
    /// A Bundle Message and its ID, packed with its neighbours into PDUs.
    Message { id: SendId, message: Message },
    /// A Transfer Cancel for the transfer number, which carries no bundle.
    Cancel(u32),
    /// A segmented transfer, cut into Transfer Segment and Transfer End
    /// messages as PDUs are packed.  Its ID is its transfer number.
    Transfer(QueuedTransfer),
    /// A bare bundle frame ([`BundleFraming::Bare`]) and the counter value
    /// of its [`SendId::Bare`]: emitted as a PDU of its own, since
    /// nothing can precede, follow, or pad it.
    BareBundle { id: u32, data: Bytes },
}

impl QueueEntry {
    /// The ID of the unsegmented bundle this entry holds, if it holds one.
    fn unsegmented_id(&self) -> Option<SendId> {
        match self {
            Self::Message { id, .. } => Some(*id),
            Self::BareBundle { id, .. } => Some(SendId::Bare(*id)),
            Self::Cancel(_) | Self::Transfer(_) => None,
        }
    }

    /// The encoded length of the next message this entry supplies, or
    /// `None` for a bare bundle frame, which is not a message and travels
    /// alone.
    fn next_message_len(&self) -> Option<usize> {
        match self {
            Self::Message { message, .. } => Some(encoded_message_len(message)),
            Self::Cancel(transfer_number) => Some(encoded_message_len(&Message::TransferCancel {
                transfer_number: *transfer_number,
            })),
            Self::Transfer(t) => Some(t.segment_len(t.offset, t.next_index)),
            Self::BareBundle { .. } => None,
        }
    }
}

/// A segmented bundle waiting in the queue.
///
/// Its segment boundaries are fixed at enqueue against the configured PDU
/// size, so what `next_pdu` will emit is decided then; but the segment
/// messages are materialised one at a time as PDUs are packed, so a queued
/// transfer costs one queue entry and a handle on the caller's buffer
/// rather than one message per segment.
struct QueuedTransfer {
    transfer_number: u32,
    data: Bytes,
    /// The hints for segment 0: the sender-derived Bundle Length followed
    /// by the caller's.  Taken when segment 0 is cut.
    first_segment_hints: Vec<HintItem>,
    /// Data bytes segment 0 may carry, reduced by its hints.
    first_capacity: usize,
    /// Data bytes every later segment may carry.
    capacity: usize,
    /// Bytes already cut into segments.
    offset: usize,
    /// The index of the next segment to cut.
    next_index: u32,
}

impl QueuedTransfer {
    /// Whether any segment has been cut (and so packed into a PDU).
    fn started(&self) -> bool {
        self.offset > 0
    }

    /// Whether every segment has been cut, the Transfer End included.
    fn finished(&self) -> bool {
        self.offset >= self.data.len()
    }

    /// The data bytes of the segment cut at `offset` with index `index`.
    fn chunk_at(&self, offset: usize, index: u32) -> usize {
        let capacity = if index == 0 {
            self.first_capacity
        } else {
            self.capacity
        };
        (self.data.len() - offset).min(capacity)
    }

    /// The encoded length of the segment message cut at `offset` with index
    /// `index`.  Sizing ahead from the cursor's own position is what
    /// `planned_len` does.
    fn segment_len(&self, offset: usize, index: u32) -> usize {
        let hints: &[HintItem] = if index == 0 {
            &self.first_segment_hints
        } else {
            &[]
        };
        segment_message_len(hints, self.chunk_at(offset, index))
    }

    /// Cut the next segment message.  The last one is a Transfer End.
    fn cut_next(&mut self) -> Message {
        let chunk = self.chunk_at(self.offset, self.next_index);
        let m = TransferSegmentMessage {
            transfer_number: self.transfer_number,
            segment_index: self.next_index,
            hints: take(&mut self.first_segment_hints),
            data: self.data.slice(self.offset..self.offset + chunk),
        };
        self.offset += chunk;
        self.next_index = self.next_index.wrapping_add(1);
        if self.finished() {
            Message::TransferEnd(m)
        } else {
            Message::TransferSegment(m)
        }
    }
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
/// on the span of outstanding numbers (see
/// [`TransferNumberAllocator`]), so draining the queue in order is what
/// frees it.
///
/// # Loss protection
///
/// This sender emits each message exactly once and packs the queue in
/// arrival order, except that a Transfer Cancel goes to the front (see
/// [`Self::cancel`]).  The repetition (Section 6) and interleaving (Section 4.1)
/// the protocol permits are not implemented here, so a lost PDU loses the
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
/// by padding, and carry hints.  [`LinkFraming::Variable`] drops the padding
/// and may additionally emit fitting bundles as bare bundle frames for a
/// peer that accepts them (see [`BundleFraming::Bare`] for the padding
/// caveat).
///
/// Every outbound unit, bare frames included, passes through the one
/// pending queue: a bare frame is emitted in arrival order behind the
/// messages queued before it, counts against the [`SendQueueDepth`], and is
/// visible to whatever schedules that queue.  Writing bare bundles to the
/// link around the sender would instead let them race and starve the
/// transfers queued here.
///
/// # Concurrency
///
/// `Sender` is single-owner: it is mutated through `&mut self`, and the
/// `tower` impls follow suit.  Under the `tower` feature,
/// `Service::poll_ready` parks the caller while the transfer window is
/// saturated or the send queue is at its [`SendQueueDepth`]; the window gate
/// applies to unsegmented bundles too, since `poll_ready` cannot see the
/// request.  Every parked task is woken when `Stream::poll_next` drains a
/// PDU or [`Self::cancel`] frees a slot or a queue entry.  `poll_next`
/// parks while the queue is empty and never yields `Ready(None)`.  Only the
/// drain frees a full window or queue, so run producers and the drain from
/// separate tasks, or from one task that selects over both; a task that
/// awaits `ready()` before polling the drain stops for good once the window
/// fills.
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
    pdu_size: PduSize,
    /// Admission bound on `pending`, in entries; enforced by the `tower`
    /// `Service::poll_ready` rather than by `enqueue` itself.
    send_queue_depth: SendQueueDepth,
    /// Owns the set of outstanding transfer numbers and the Section 5 window
    /// rule.  [`Self::cancel`] and the End-packing release in
    /// [`Self::next_pdu`] only act on numbers it reports as outstanding.
    allocator: TransferNumberAllocator,
    link_framing: LinkFraming,
    pending: VecDeque<QueueEntry>,
    /// The counter value for the next [`SendId::Message`] or
    /// [`SendId::Bare`]; wraps.
    next_bundle_id: u32,
    /// Every task parked in the `tower` `Service::poll_ready`, woken when a
    /// window slot frees or the send queue drains below its depth.  A list
    /// rather than a slot so that several producers sharing the sender
    /// through a mutex are all woken; re-registration by a task already
    /// present is deduplicated with `Waker::will_wake`.
    #[cfg(feature = "tower")]
    enqueue_wakers: Vec<Waker>,
    /// The task parked in `Stream::poll_next`, woken when a new entry is
    /// pushed to `pending`.  Single-slot: the drain has one consumer.
    #[cfg(feature = "tower")]
    drain_waker: Option<Waker>,
}

/// Summarises the queue rather than printing it: the queued bundles'
/// bytes would make the output as large as the backlog.
impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Sender");
        d.field("pdu_size", &self.pdu_size)
            .field("send_queue_depth", &self.send_queue_depth)
            .field("link_framing", &self.link_framing)
            .field("window_size", &self.allocator.window_size())
            .field("transfers_outstanding", &self.allocator.in_progress())
            .field("window_available", &self.is_window_available())
            .field("queued", &self.pending.len())
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
    /// See [`TransferNumberAllocator::new`] for the spec-recommended choice
    /// of this value, and `Sender::try_from_rng` and `Sender::from_rng`
    /// (under the `rand` feature) for the common case of seeding from an RNG.
    pub fn new(config: SenderConfig, initial_transfer_number: u32) -> Self {
        Self {
            pdu_size: config.pdu_size,
            send_queue_depth: config.send_queue_depth,
            allocator: TransferNumberAllocator::new(config.window_size, initial_transfer_number),
            link_framing: config.link_framing,
            pending: VecDeque::new(),
            next_bundle_id: 0,
            #[cfg(feature = "tower")]
            enqueue_wakers: Vec::new(),
            #[cfg(feature = "tower")]
            drain_waker: None,
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

    /// Append `entry` to the pending queue and wake the drain.
    fn queue(&mut self, entry: QueueEntry) {
        self.pending.push_back(entry);
        self.wake_drain();
    }

    /// Wake every task parked on a `Service::poll_ready` that returned
    /// `Pending` because the window was full or the send queue was at
    /// depth, once both have room: the same predicate `poll_ready` gates
    /// on, so a woken task finds the service ready.  No-op without the
    /// `tower` feature.
    #[cfg(feature = "tower")]
    fn wake_enqueue(&mut self) {
        if self.is_window_available() && !self.is_send_queue_full() {
            for w in self.enqueue_wakers.drain(..) {
                w.wake();
            }
        }
    }
    #[cfg(not(feature = "tower"))]
    fn wake_enqueue(&mut self) {}

    /// Wake any task parked on a `Stream::poll_next` that returned `Pending`
    /// because `pending` was empty. No-op without the `tower` feature.
    #[cfg(feature = "tower")]
    fn wake_drain(&mut self) {
        if let Some(w) = self.drain_waker.take() {
            w.wake();
        }
    }
    #[cfg(not(feature = "tower"))]
    fn wake_drain(&mut self) {}

    /// Whether a segmented bundle could currently be admitted without
    /// violating the transfer window (see
    /// [`TransferNumberAllocator::can_allocate`]).
    ///
    /// The `tower` `Service::poll_ready` uses this as its window gate, so it
    /// is exactly the predicate [`Self::enqueue`] applies when segmenting.
    pub fn is_window_available(&self) -> bool {
        self.allocator.can_allocate()
    }

    /// Whether the pending queue has reached its configured
    /// [`SendQueueDepth`].
    ///
    /// The `tower` `Service::poll_ready` uses this as its admission gate:
    /// unsegmented bundles take no window slot, so without it the queue
    /// would grow without bound whenever the drain side is slower.  Direct
    /// [`Self::enqueue`] callers can poll it to pace themselves the same
    /// way, draining [`Self::next_pdu`] when it reports full.
    pub fn is_send_queue_full(&self) -> bool {
        self.pending.len() >= self.send_queue_depth.get()
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
    /// without segmentation and a [`SendId::Message`] is returned.
    /// Otherwise, it is split into Transfer Segment and Transfer End messages
    /// and [`SendId::Transfer`] carries the allocated transfer
    /// number.
    ///
    /// Under [`BundleFraming::Bare`], a bundle that fits in a PDU, carries no
    /// caller hints, and begins with a bundle-reserved byte is instead queued
    /// as a bare frame and a [`SendId::Bare`] is returned.
    ///
    /// Caller hints from `options` ride on the Bundle message or the first
    /// segment (hints are transfer-scoped, Section 7.2); the sender derives
    /// and attaches the Bundle Length hint itself when segmenting.
    ///
    /// An empty `data` is rejected with [`Error::Empty`]: it cannot be
    /// a valid bundle (Section 8.1), and nothing is queued.
    ///
    /// Segments are zero-copy views into `data`.  A [`Bytes`] made from a
    /// fresh `Vec` allocates its shared header the first time it is
    /// sliced, so a segmented bundle costs at most that one allocation.
    pub fn enqueue(&mut self, data: Bytes, options: SendOptions) -> Result<SendId> {
        if data.is_empty() {
            return Err(Error::Empty);
        }

        // The sender owns the Bundle Length hint, so a caller-supplied one
        // is discarded.
        let mut hints = options.hints;
        hints.remove(HintType::BUNDLE_LENGTH);

        let bundle_len = data.len();

        if self.bare_bundles()
            && hints.is_empty()
            && bundle_len <= self.pdu_size.get()
            && frame_kind(&data) != FrameKind::BtpuPdu
        {
            // A bare frame needs no header, so it may use the whole PDU.
            let id = self.take_bundle_id();
            self.queue(QueueEntry::BareBundle { id, data });
            return Ok(SendId::Bare(id));
        }

        if HEADER_SIZE + hints.encoded_len() + bundle_len <= self.pdu_size.get() {
            // Fits in a single Bundle message.
            let id = SendId::Message(self.take_bundle_id());
            let entry = self.message_entry(
                id,
                Message::Bundle {
                    hints: hints.into_vec(),
                    data,
                },
            );
            self.queue(entry);
            return Ok(id);
        }

        // Segment the bundle.  Size the segments before taking a transfer
        // number so a PDU too small to carry them never touches the window.
        let capacity = self.pdu_size.get().saturating_sub(SEGMENT_FRAMING);
        hints.insert(HintItem::BundleLength(bundle_len as u64));
        let first_segment_framing = SEGMENT_FRAMING + hints.encoded_len();
        let first_capacity = self.pdu_size.get().saturating_sub(first_segment_framing);

        if capacity == 0 || first_capacity == 0 {
            return Err(Error::PduTooSmall {
                required: first_segment_framing + 1,
                pdu_size: self.pdu_size.get(),
            });
        }
        // Segment 0 carries first_capacity bytes and every later segment
        // `capacity`; indices must fit the 32-bit field (Section 8.2).
        let later_segments = bundle_len.saturating_sub(first_capacity).div_ceil(capacity);
        if u32::try_from(later_segments).is_err() {
            return Err(Error::TooManySegments {
                len: bundle_len,
                pdu_size: self.pdu_size.get(),
            });
        }

        let transfer_number = self.allocator.allocate()?;
        self.queue(QueueEntry::Transfer(QueuedTransfer {
            transfer_number,
            data,
            first_segment_hints: hints.into_vec(),
            first_capacity,
            capacity,
            offset: 0,
            next_index: 0,
        }));
        Ok(SendId::Transfer(transfer_number))
    }

    /// Abandon a bundle the sender has not finished emitting.
    ///
    /// For a [`SendId::Transfer`], the transfer's window slot is
    /// freed and its segments not yet packed are discarded.  If any had
    /// already been emitted, a Transfer Cancel message is queued at the
    /// front, ahead of everything already waiting, so the receiver discards
    /// what it holds (Section 4.2) as soon as the next PDU arrives; if none
    /// had, the receiver never learned of the transfer and no Cancel is
    /// sent.
    ///
    /// A [`SendId::Message`] or [`SendId::Bare`] still
    /// in the queue is removed from it.  Such a bundle travels whole, so the
    /// receiver has seen none of it and nothing is sent in its place.
    ///
    /// Returns whether the bundle was cancelled.  Returns `false`, changing
    /// nothing, if the sender has nothing left to emit for `id`: it was
    /// never issued, was already cancelled, or its last bytes have been
    /// packed, in which case a PDU has listed it with
    /// [`Carried::completes`] set.
    ///
    /// Costs a scan of the send queue, and for a transfer a scan of the
    /// outstanding transfer numbers as well (at most the window size).
    pub fn cancel(&mut self, id: SendId) -> bool {
        let cancelled = match id {
            SendId::Transfer(transfer_number) => self.cancel_transfer(transfer_number),
            SendId::Message(_) | SendId::Bare(_) => self
                .pending
                .iter()
                .position(|e| e.unsegmented_id() == Some(id))
                .and_then(|at| self.pending.remove(at))
                .is_some(),
        };
        if cancelled {
            // A window slot or a queue entry freed.  No drain wake is
            // needed: cancelling only ever removes or replaces entries.
            self.wake_enqueue();
        }
        cancelled
    }

    /// The [`SendId::Transfer`] case of [`Self::cancel`].
    fn cancel_transfer(&mut self, transfer_number: u32) -> bool {
        if !self.allocator.release(transfer_number) {
            return false;
        }

        // An outstanding transfer is one queue entry until its End is
        // packed.  Segments are cut in index order, so a transfer that has
        // not started means nothing of it has been emitted.
        let at = self.pending.iter().position(
            |e| matches!(e, QueueEntry::Transfer(t) if t.transfer_number == transfer_number),
        );
        let nothing_emitted = match at.and_then(|at| self.pending.remove(at)) {
            Some(QueueEntry::Transfer(t)) => !t.started(),
            _ => false,
        };
        if !nothing_emitted {
            // At the front, so the receiver can drop what it holds without
            // waiting out the backlog.  Emitting a smaller number early
            // cannot raise the greatest emitted, so Section 5 still holds.
            self.pending.push_front(QueueEntry::Cancel(transfer_number));
        }
        true
    }

    /// Pack pending messages into a PDU of at most `pdu_size` bytes.
    ///
    /// Returns `None` if nothing is pending.  Under
    /// [`LinkFraming::FixedSize`] the PDU is padded to exactly `pdu_size`
    /// bytes; under [`LinkFraming::Variable`] it holds only the packed
    /// messages.  A queued bare bundle frame is returned as-is, on its own,
    /// sharing the enqueued buffer.
    ///
    /// [`Pdu::carried`] names every bundle with bytes in the PDU and flags
    /// those whose last bytes it carries.  A CLA that reports per-bundle
    /// outcomes can treat a bundle as sent once the PDU flagging it
    /// [`Carried::completes`] is written, and as failed if it
    /// [`Self::cancel`]s it first (a cancelled bundle is never flagged) or
    /// a write of any PDU carrying it fails.  [`Self::next_pdu_into`] does
    /// the same into a reused list.
    ///
    /// The list starts inline, so it allocates only for a PDU carrying
    /// more than [`CarriedList::INLINE`] bundles.
    ///
    /// Packing a Transfer End releases its transfer's window slot (see
    /// [`Sender`]).
    pub fn next_pdu(&mut self) -> Option<Pdu> {
        let mut carried = CarriedList::new();
        let data = self.next_pdu_into(&mut carried)?;
        Some(Pdu { data, carried })
    }

    /// [`Self::next_pdu`], writing the carried bundles into `carried`
    /// rather than a new list, so a caller draining a busy link allocates
    /// nothing for the list once it has grown to the largest PDU seen, or
    /// nothing at all if created with
    /// [`CarriedList::with_capacity`]`(pdu_size / 32 + 1)` (see
    /// [`CarriedList`] for that bound).
    ///
    /// `carried` is cleared first, keeping its heap buffer if it has one,
    /// so after the call it holds exactly this PDU's entries, and is left
    /// empty when `None` is returned.
    pub fn next_pdu_into(&mut self, carried: &mut CarriedList) -> Option<Bytes> {
        carried.clear();
        let pdu = self.pack(carried)?;

        // Draining frees send-queue capacity (and possibly a window slot);
        // wake any task parked on `poll_ready`.
        self.wake_enqueue();

        Some(pdu)
    }

    /// Pack the queue's next PDU, recording the bundles it carries in
    /// `carried`, or return `None` if nothing is pending.
    ///
    /// The loop is driven by the queue, not by a count planned up front:
    /// [`Self::next_source`] names the entry that supplies each message,
    /// and an entry leaves the queue only once its last message is packed.
    /// The first message always goes in, so every PDU makes progress.
    /// Nothing larger than an empty PDU is ever queued (a Bundle Message is
    /// only made for a bundle that fits with its hints, `enqueue` cuts a
    /// transfer's segments to fit or refuses the bundle, and a Transfer
    /// Cancel is smaller than any segment of the transfer it cancels), but
    /// were it otherwise the message would go out as one oversized PDU
    /// rather than stall the queue.
    fn pack(&mut self, carried: &mut CarriedList) -> Option<Bytes> {
        if self.pending.is_empty() {
            return None;
        }
        let pdu_size = self.pdu_size.get();
        let fixed_size = self.link_framing == LinkFraming::FixedSize;
        let mut buf = BytesMut::with_capacity(if fixed_size {
            pdu_size
        } else {
            self.planned_len(pdu_size)
        });
        while let Some(at) = self.next_source(buf.len(), pdu_size) {
            let consumed = match &mut self.pending[at] {
                QueueEntry::BareBundle { id, data } => {
                    // Chosen only for an empty PDU; returned as-is, alone.
                    carried.push(Carried {
                        id: SendId::Bare(*id),
                        completes: true,
                    });
                    let data = data.clone();
                    self.pending.remove(at);
                    return Some(data);
                }
                QueueEntry::Message { id, message } => {
                    carried.push(Carried {
                        id: *id,
                        completes: true,
                    });
                    encode_queued(message, &mut buf);
                    true
                }
                QueueEntry::Cancel(transfer_number) => {
                    let transfer_number = *transfer_number;
                    encode_queued(&Message::TransferCancel { transfer_number }, &mut buf);
                    true
                }
                QueueEntry::Transfer(t) => {
                    encode_queued(&t.cut_next(), &mut buf);
                    let completes = t.finished();
                    // A transfer's segments in one PDU are consecutive, so
                    // they share one entry.
                    let id = SendId::Transfer(t.transfer_number);
                    match carried.last_mut() {
                        Some(last) if last.id == id => last.completes = completes,
                        _ => carried.push(Carried { id, completes }),
                    }
                    if completes {
                        // The transfer's End is packed; nothing further will
                        // be emitted for it, so its slot is free.
                        self.allocator.release(t.transfer_number);
                    }
                    completes
                }
            };
            if consumed {
                self.pending.remove(at);
            }
        }
        if fixed_size {
            pad_pdu(&mut buf, pdu_size);
        }
        Some(buf.freeze())
    }

    /// The queue position of the entry that supplies the next message of a
    /// PDU holding `used` bytes, or `None` to end the PDU.
    ///
    /// An empty PDU takes the front entry whatever its size, which is what
    /// guarantees progress.  A non-empty one takes the front entry's next
    /// message only if it fits the room left, and never a bare bundle frame.
    /// This is the choice an interleaving scheduler would make; for now it
    /// is first in, first out.
    fn next_source(&self, used: usize, pdu_size: usize) -> Option<usize> {
        let front = self.pending.front()?;
        if used == 0 {
            return Some(0);
        }
        let len = front.next_message_len()?;
        (used + len <= pdu_size).then_some(0)
    }

    /// The encoded size of the messages the next PDU will pack, so a
    /// [`LinkFraming::Variable`] buffer is allocated once at the right size.
    /// It mirrors [`Self::next_source`] but only sizes the buffer: were the
    /// two to disagree, the buffer would grow or carry spare capacity, and
    /// the PDU would be the same.
    fn planned_len(&self, pdu_size: usize) -> usize {
        let mut total = 0;
        for entry in &self.pending {
            match entry {
                QueueEntry::Transfer(t) => {
                    let (mut offset, mut index) = (t.offset, t.next_index);
                    while offset < t.data.len() {
                        let len = t.segment_len(offset, index);
                        if total + len > pdu_size {
                            return total;
                        }
                        total += len;
                        offset += t.chunk_at(offset, index);
                        index = index.wrapping_add(1);
                    }
                }
                _ => match entry.next_message_len() {
                    Some(len) if total + len <= pdu_size => total += len,
                    _ => return total,
                },
            }
        }
        total
    }

    /// Returns `true` if there are messages pending for transmission.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether `transfer_number` is an outstanding transfer of this sender:
    /// segmented, and its End not yet packed or the transfer cancelled.
    ///
    /// Costs a scan of the outstanding transfer numbers, at most the window
    /// size.
    pub fn is_transfer_outstanding(&self, transfer_number: u32) -> bool {
        self.allocator.is_outstanding(transfer_number)
    }

    /// Wrap a Bundle Message for the queue, checking that it fits an empty
    /// PDU (see [`Self::pack`]).
    fn message_entry(&self, id: SendId, message: Message) -> QueueEntry {
        debug_assert!(
            encoded_message_len(&message) <= self.pdu_size.get(),
            "message larger than the PDU"
        );
        QueueEntry::Message { id, message }
    }

    /// Take the next [`SendId::Message`] or
    /// [`SendId::Bare`] counter value.
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

    /// Whether fitting bundles may be emitted as bare bundle frames.
    fn bare_bundles(&self) -> bool {
        self.link_framing
            == LinkFraming::Variable {
                bundle_framing: BundleFraming::Bare,
            }
    }
}

/// Append a queued message to a PDU being packed.
fn encode_queued(message: &Message, buf: &mut BytesMut) {
    encode_message(message, buf).expect("queued messages are validated at enqueue");
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::codec::hint::HintValue;

    #[test]
    fn unsegmented_ids_wrap_across_message_and_bare() {
        let mut s = Sender::new(
            SenderConfig {
                link_framing: LinkFraming::Variable {
                    bundle_framing: BundleFraming::Bare,
                },
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
            Ok(SendId::Bare(u32::MAX))
        );
        assert_eq!(s.enqueue(bare.clone(), hinted), Ok(SendId::Message(0)));
        assert_eq!(s.enqueue(bare, SendOptions::default()), Ok(SendId::Bare(1)));
    }

    #[test]
    fn oversized_queued_message_goes_out_alone_and_the_queue_moves_on() {
        const PDU: usize = 64;
        let mut s = Sender::new(
            SenderConfig {
                pdu_size: PduSize::new(PDU).unwrap(),
                link_framing: LinkFraming::Variable {
                    bundle_framing: BundleFraming::Message,
                },
                ..SenderConfig::default()
            },
            0,
        );
        // Break the invariant `enqueue` keeps, bypassing `message_entry`.
        let oversized = Message::Bundle {
            hints: Vec::new(),
            data: Bytes::from(vec![0; 2 * PDU]),
        };
        let oversized_len = encoded_message_len(&oversized);
        s.pending.push_back(QueueEntry::Message {
            id: SendId::Message(100),
            message: oversized,
        });
        let small = s
            .enqueue(Bytes::from_static(&[1, 2, 3]), SendOptions::default())
            .unwrap();

        let pdu = s.next_pdu().unwrap();
        assert_eq!(pdu.data.len(), oversized_len);
        assert_eq!(
            &pdu.carried[..],
            &[Carried {
                id: SendId::Message(100),
                completes: true,
            }]
        );
        let pdu = s.next_pdu().unwrap();
        assert_eq!(
            &pdu.carried[..],
            &[Carried {
                id: small,
                completes: true,
            }]
        );
        assert_eq!(s.next_pdu(), None);
    }
}
