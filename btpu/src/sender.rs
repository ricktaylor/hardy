use alloc::{collections::VecDeque, vec, vec::Vec};
#[cfg(feature = "tower")]
use core::task::Waker;
use core::{fmt, mem::take, num::NonZeroUsize, str::FromStr};

use bytes::{Bytes, BytesMut};
#[cfg(feature = "rand")]
use rand_core::{Rng, TryRng};

use crate::{
    OutOfRange,
    ParseError,
    codec::{
        encode_message, encoded_message_len,
        header::{HEADER_SIZE, MAX_CONTENT_LENGTH},
        hint::{BUNDLE_LENGTH_HINT, HintItem, ValidationError, encoded_hints_len, validate_hints},
        message::{FrameKind, Message, TransferSegmentMessage, frame_kind},
        pad_pdu,
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
    #[error("Empty bundle")]
    EmptyBundle,

    /// A caller-supplied hint item is not encodable (type or value out of
    /// range).  Checked at [`Sender::enqueue`] so the fault is attributed to
    /// the offending bundle rather than surfacing later during PDU packing.
    #[error(transparent)]
    InvalidHint(#[from] ValidationError),

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
    /// and beyond; see [`PduSize::MIN`].
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
    TooManySegments { len: usize, pdu_size: usize },
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
    /// The minimum only keeps the sender live; a usable PDU is larger.  A
    /// Bundle Message spends [`HEADER_SIZE`] bytes before its bundle, and
    /// segmenting needs at least 16 bytes, more for longer bundles and for
    /// caller hints.  `enqueue` refuses a bundle it cannot segment with
    /// [`Error::PduTooSmall`], which gives the exact figure.
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
    /// [`PduSize::DEFAULT`].
    fn default() -> Self {
        Self::DEFAULT
    }
}

forward_integer_fmt!(PduSize);

impl FromStr for PduSize {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let v = s.parse::<usize>().map_err(|source| ParseError::Syntax {
            name: Self::NAME,
            source,
        })?;
        Ok(Self::try_from(v)?)
    }
}

impl TryFrom<usize> for PduSize {
    type Error = OutOfRange;

    fn try_from(v: usize) -> Result<Self, OutOfRange> {
        Self::new(v).ok_or(OutOfRange {
            name: Self::NAME,
            value: v as u64,
            min: Self::MIN.get() as u64,
            max: Some(Self::MAX.get() as u64),
        })
    }
}

impl From<PduSize> for usize {
    fn from(p: PduSize) -> usize {
        p.get()
    }
}

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

    /// 64 queued bundles.
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
    /// [`SendQueueDepth::DEFAULT`].
    fn default() -> Self {
        Self::DEFAULT
    }
}

forward_integer_fmt!(SendQueueDepth);

impl FromStr for SendQueueDepth {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let v = s.parse::<usize>().map_err(|source| ParseError::Syntax {
            name: Self::NAME,
            source,
        })?;
        Ok(Self::try_from(v)?)
    }
}

impl TryFrom<usize> for SendQueueDepth {
    type Error = OutOfRange;

    fn try_from(v: usize) -> Result<Self, OutOfRange> {
        Self::new(v).ok_or(OutOfRange {
            name: Self::NAME,
            value: v as u64,
            min: Self::MIN.get() as u64,
            max: None,
        })
    }
}

impl From<NonZeroUsize> for SendQueueDepth {
    fn from(n: NonZeroUsize) -> Self {
        Self(n)
    }
}

impl From<SendQueueDepth> for NonZeroUsize {
    fn from(d: SendQueueDepth) -> NonZeroUsize {
        d.0
    }
}

impl From<SendQueueDepth> for usize {
    fn from(d: SendQueueDepth) -> usize {
        d.get()
    }
}

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
    /// The pending-queue admission bound.  Default: 64 messages.
    pub send_queue_depth: SendQueueDepth,
    /// The link's framing discipline.  Default: fixed-size frames.
    pub link_framing: LinkFraming,
}

/// Options for [`Sender::enqueue`].
///
/// `Default`-constructible; `SendOptions::default()` sends with no
/// caller-supplied hints.
///
/// Deliberately **not** `#[non_exhaustive]`: `hints` is already the
/// catch-all, since any metadata the sender needn't specially understand
/// rides through as [`HintItem::Unknown`].  A new structured field is only
/// ever added for a capability the sender actively implements (a priority
/// selector, a repetition policy), which is a coordinated change to this
/// crate and its callers together; the compile break on the struct literal
/// is the useful checklist.
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    /// Hint items to attach to the transfer, carried on the Bundle message
    /// (unsegmented) or the first segment (segmented) alongside the
    /// sender-derived Bundle Length hint.  A caller-supplied
    /// [`HintItem::BundleLength`] is discarded: the sender derives the
    /// truthful value itself.
    pub hints: Vec<HintItem>,
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
pub enum BundleTransferId {
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
const _: () = assert!(size_of::<BundleTransferId>() == 8);

/// A bundle with bytes in a PDU, as listed by [`Pdu::bundles`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Carried {
    /// The bundle, as [`Sender::enqueue`] reported it.
    pub id: BundleTransferId,
    /// Whether this PDU carries the bundle's last bytes: always for a
    /// Bundle Message or bare frame, and for a segmented bundle when the
    /// PDU holds its Transfer End.  Once the PDU holding it is written,
    /// the sender will emit nothing further for the bundle.
    pub completes: bool,
}

/// A packed PDU and the bundles it carries, returned by [`Sender::next_pdu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    /// The PDU, ready for the link.
    pub data: Bytes,
    /// Every bundle with bytes in `data`, once each, in the order packed.
    /// Empty for a PDU holding only a Transfer Cancel.
    pub bundles: Vec<Carried>,
}

/// One unit of the pending send queue.  No `Debug`: it holds bundle bytes.
enum QueueEntry {
    /// A single BTP-U message, packed with its neighbours into PDUs: a
    /// Bundle Message with its ID, or a Transfer Cancel, which carries no
    /// bundle.
    Message {
        id: Option<BundleTransferId>,
        message: Message,
    },
    /// A segmented transfer, cut into Transfer Segment and Transfer End
    /// messages as PDUs are packed.  Its ID is its transfer number.
    Transfer(QueuedTransfer),
    /// A bare bundle frame ([`BundleFraming::Bare`]) and the counter value
    /// of its [`BundleTransferId::Bare`]: emitted as a PDU of its own, since
    /// nothing can precede, follow, or pad it.
    BareBundle { id: u32, data: Bytes },
}

impl QueueEntry {
    /// The ID of the unsegmented bundle this entry holds, if it holds one.
    fn unsegmented_id(&self) -> Option<BundleTransferId> {
        match self {
            Self::Message { id, .. } => *id,
            Self::BareBundle { id, .. } => Some(BundleTransferId::Bare(*id)),
            Self::Transfer(_) => None,
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
    /// `index`.  Planning ahead from the cursor's own position is what
    /// `plan_pdu` does.
    fn segment_len(&self, offset: usize, index: u32) -> usize {
        let hints_len = if index == 0 {
            encoded_hints_len(&self.first_segment_hints)
        } else {
            0
        };
        HEADER_SIZE + hints_len + 8 + self.chunk_at(offset, index)
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
/// `Sender` is designed for **single-owner** use: one task at a time mutates
/// it via `&mut self`. The `tower::Service` and `futures_core::Stream` impls
/// (under the `tower` feature) follow this contract. To share a `Sender`
/// across tasks, wrap it in `Arc<Mutex<_>>`; the outer synchronisation
/// serialises the state, and every task parked in `Service::poll_ready` is
/// woken when capacity frees, so several producers may fan in through one
/// mutex.  Admission is not reserved between `poll_ready` and `call` (or
/// between [`Self::is_window_available`] and [`Self::enqueue`]), so a
/// producer must do both under one lock acquisition and must release the
/// lock before parking; a `WindowFull` from `enqueue` after a positive
/// check means another producer got there first, and the right response is
/// to check again.  The `Stream` drain has one consumer.  Do **not** use
/// `tower::buffer::Buffer`: it moves the `Sender` into a worker task and
/// exposes only the `Service` half, stranding the `Stream` drain and
/// [`Self::cancel`], so PDUs never leave and window slots never free.
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
    /// The counter value for the next [`BundleTransferId::Message`] or
    /// [`BundleTransferId::Bare`]; wraps.
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

    /// Wake every task parked on a `Service::poll_ready` that returned
    /// `Pending` because the window was full or the send queue was at
    /// depth. No-op without the `tower` feature.
    #[cfg(feature = "tower")]
    fn wake_enqueue(&mut self) {
        for w in self.enqueue_wakers.drain(..) {
            w.wake();
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
    /// [`Pdu::bundles`] will report it.
    ///
    /// If the bundle fits in a single PDU (as a Bundle message), it is emitted
    /// without segmentation and a [`BundleTransferId::Message`] is returned.
    /// Otherwise, it is split into Transfer Segment and Transfer End messages
    /// and [`BundleTransferId::Transfer`] carries the allocated transfer
    /// number.
    ///
    /// Under [`BundleFraming::Bare`], a bundle that fits in a PDU, carries no
    /// caller hints, and begins with a bundle-reserved byte is instead queued
    /// as a bare frame and a [`BundleTransferId::Bare`] is returned.
    ///
    /// Caller hints from `options` ride on the Bundle message or the first
    /// segment (hints are transfer-scoped, Section 7.2); the sender derives
    /// and attaches the Bundle Length hint itself when segmenting.
    ///
    /// An empty `data` is rejected with [`Error::EmptyBundle`]: it cannot be
    /// a valid bundle (Section 8.1), and nothing is queued.
    pub fn enqueue(&mut self, data: Bytes, options: SendOptions) -> Result<BundleTransferId> {
        if data.is_empty() {
            return Err(Error::EmptyBundle);
        }

        // Validate up front: a bad hint must fail this bundle, not panic
        // later when next_pdu encodes the queued message.  The sender owns
        // the Bundle Length hint, so a caller-supplied one is discarded.
        let mut caller_hints = options.hints;
        caller_hints.retain(|h| h.hint_type() != BUNDLE_LENGTH_HINT);
        validate_hints(&caller_hints)?;

        let bundle_len = data.len();

        if self.bare_bundles()
            && caller_hints.is_empty()
            && bundle_len <= self.pdu_size.get()
            && frame_kind(&data) != FrameKind::BtpuPdu
        {
            // A bare frame needs no header, so it may use the whole PDU.
            let id = self.take_bundle_id();
            self.pending.push_back(QueueEntry::BareBundle { id, data });
            self.wake_drain();
            return Ok(BundleTransferId::Bare(id));
        }

        let caller_hints_len = encoded_hints_len(&caller_hints);
        let max_bundle_content = self.max_single_bundle_content();

        if bundle_len + caller_hints_len <= max_bundle_content {
            // Fits in a single Bundle message.
            let id = BundleTransferId::Message(self.take_bundle_id());
            let entry = self.message_entry(
                Some(id),
                Message::Bundle {
                    hints: caller_hints,
                    data,
                },
            );
            self.pending.push_back(entry);
            self.wake_drain();
            return Ok(id);
        }

        // Segment the bundle.  Size the segments before taking a transfer
        // number so a PDU too small to carry them never touches the window.
        let capacity = self.max_segment_data();
        let mut first_segment_hints = vec![HintItem::BundleLength(bundle_len as u64)];
        first_segment_hints.extend(caller_hints);
        let first_segment_framing = HEADER_SIZE + 8 + encoded_hints_len(&first_segment_hints);
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
        self.pending.push_back(QueueEntry::Transfer(QueuedTransfer {
            transfer_number,
            data,
            first_segment_hints,
            first_capacity,
            capacity,
            offset: 0,
            next_index: 0,
        }));
        self.wake_drain();
        Ok(BundleTransferId::Transfer(transfer_number))
    }

    /// Abandon a bundle the sender has not finished emitting.
    ///
    /// For a [`BundleTransferId::Transfer`], the transfer's window slot is
    /// freed and its segments not yet packed are discarded.  If any had
    /// already been emitted, a Transfer Cancel message is queued at the
    /// front, ahead of everything already waiting, so the receiver discards
    /// what it holds (Section 4.2) as soon as the next PDU arrives; if none
    /// had, the receiver never learned of the transfer and no Cancel is
    /// sent.
    ///
    /// A [`BundleTransferId::Message`] or [`BundleTransferId::Bare`] still
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
    pub fn cancel(&mut self, id: BundleTransferId) -> bool {
        let cancelled = match id {
            BundleTransferId::Transfer(transfer_number) => self.cancel_transfer(transfer_number),
            BundleTransferId::Message(_) | BundleTransferId::Bare(_) => {
                match self
                    .pending
                    .iter()
                    .position(|e| e.unsegmented_id() == Some(id))
                {
                    Some(at) => {
                        self.pending.remove(at);
                        true
                    }
                    None => false,
                }
            }
        };
        if cancelled {
            // A window slot or a queue entry freed.  No drain wake is
            // needed: cancelling only ever removes or replaces entries.
            self.wake_enqueue();
        }
        cancelled
    }

    /// The [`BundleTransferId::Transfer`] case of [`Self::cancel`].
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
            let entry = self.message_entry(None, Message::TransferCancel { transfer_number });
            self.pending.push_front(entry);
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
    /// [`Pdu::bundles`] names every bundle with bytes in the PDU and flags
    /// those whose last bytes it carries.  A CLA that reports per-bundle
    /// outcomes can treat a bundle as sent once the PDU flagging it
    /// [`Carried::completes`] is written, and as failed if it
    /// [`Self::cancel`]s it first (a cancelled bundle is never flagged) or
    /// a write of any PDU carrying it fails.  [`Self::next_pdu_into`] does
    /// the same into a reused list.
    ///
    /// Packing a Transfer End releases its transfer's window slot (see
    /// [`Sender`]).
    pub fn next_pdu(&mut self) -> Option<Pdu> {
        let mut bundles = Vec::new();
        let data = self.next_pdu_into(&mut bundles)?;
        Some(Pdu { data, bundles })
    }

    /// [`Self::next_pdu`], writing the carried bundles into `bundles`
    /// rather than a new list, so a caller draining a busy link allocates
    /// the list once.
    ///
    /// `bundles` is cleared first, so after the call it holds exactly this
    /// PDU's entries, and is left empty when `None` is returned.
    pub fn next_pdu_into(&mut self, bundles: &mut Vec<Carried>) -> Option<Bytes> {
        bundles.clear();
        let pdu = match self.pending.front()? {
            QueueEntry::BareBundle { id, data } => {
                bundles.push(Carried {
                    id: BundleTransferId::Bare(*id),
                    completes: true,
                });
                let data = data.clone();
                self.pending.pop_front();
                data
            }
            QueueEntry::Message { .. } | QueueEntry::Transfer(_) => self.pack_messages(bundles),
        };

        // Draining frees send-queue capacity (and possibly a window slot);
        // wake any task parked on `poll_ready`.
        self.wake_enqueue();

        Some(pdu)
    }

    /// Pack the run of messages at the front of the queue that fits one PDU,
    /// recording the bundles it carries in `bundles`.  The front entry must
    /// be a message or a transfer.
    fn pack_messages(&mut self, bundles: &mut Vec<Carried>) -> Bytes {
        let pdu_size = self.pdu_size.get();
        let (count, total) = self.plan_pdu(pdu_size);
        // The front message always fits an empty PDU, because nothing larger
        // is ever queued: a Bundle Message is only made for a bundle that
        // fits with its hints, `enqueue` cuts a transfer's segments to fit
        // (refusing the bundle if even one cannot), and a Transfer Cancel is
        // smaller than any segment of the transfer it cancels.  The
        // `PduSize` is fixed for the sender's life, so none of that goes
        // stale.  If it did not hold, this would emit empty PDUs forever
        // without draining the queue.
        debug_assert!(count > 0, "queued message larger than a PDU");

        let mut buf = BytesMut::with_capacity(if self.link_framing == LinkFraming::FixedSize {
            pdu_size
        } else {
            total
        });
        for _ in 0..count {
            let msg = match self.pending.pop_front() {
                Some(QueueEntry::Message { id, message }) => {
                    if let Some(id) = id {
                        bundles.push(Carried {
                            id,
                            completes: true,
                        });
                    }
                    message
                }
                Some(QueueEntry::Transfer(mut t)) => {
                    let msg = t.cut_next();
                    let transfer_number = t.transfer_number;
                    let completes = t.finished();
                    // A transfer stays at the front until its End is cut, so
                    // its segments in this PDU are consecutive and share one
                    // entry.
                    let id = BundleTransferId::Transfer(transfer_number);
                    match bundles.last_mut() {
                        Some(last) if last.id == id => last.completes = completes,
                        _ => bundles.push(Carried { id, completes }),
                    }
                    if completes {
                        // The transfer's End has left the queue; nothing
                        // further will be emitted for it, so its slot is
                        // free.
                        self.allocator.release(transfer_number);
                    } else {
                        self.pending.push_front(QueueEntry::Transfer(t));
                    }
                    msg
                }
                // `plan_pdu` counted only messages and transfer segments, so
                // neither a bare bundle nor an empty queue is reached; put
                // back anything taken and end the PDU.
                other => {
                    if let Some(entry) = other {
                        self.pending.push_front(entry);
                    }
                    break;
                }
            };
            encode_message(&msg, &mut buf).expect("queued messages are validated at enqueue");
        }
        if self.link_framing == LinkFraming::FixedSize {
            pad_pdu(&mut buf, pdu_size);
        }
        buf.freeze()
    }

    /// How many leading messages fit one PDU, and their total encoded size,
    /// so the buffer is allocated once at the right size.  A queued transfer
    /// contributes as many of its remaining segments as fit; a bare frame
    /// travels alone, so the run stops there.
    fn plan_pdu(&self, pdu_size: usize) -> (usize, usize) {
        let mut total = 0;
        let mut count = 0;
        for entry in &self.pending {
            match entry {
                QueueEntry::Message { message, .. } => {
                    let len = encoded_message_len(message);
                    if total + len > pdu_size {
                        return (count, total);
                    }
                    total += len;
                    count += 1;
                }
                QueueEntry::Transfer(t) => {
                    let (mut offset, mut index) = (t.offset, t.next_index);
                    while offset < t.data.len() {
                        let len = t.segment_len(offset, index);
                        if total + len > pdu_size {
                            return (count, total);
                        }
                        total += len;
                        count += 1;
                        offset += t.chunk_at(offset, index);
                        index = index.wrapping_add(1);
                    }
                }
                QueueEntry::BareBundle { .. } => return (count, total),
            }
        }
        (count, total)
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

    /// Wrap a message for the queue, upholding the invariant
    /// `pack_messages` relies on: every queued message fits an empty PDU.
    /// `enqueue` sizes messages against the PDU, and a Cancel is only
    /// queued for a transfer whose segments fitted, so this can only fail
    /// for a future caller that forgets to.
    fn message_entry(&self, id: Option<BundleTransferId>, message: Message) -> QueueEntry {
        debug_assert!(
            encoded_message_len(&message) <= self.pdu_size.get(),
            "message larger than the PDU"
        );
        QueueEntry::Message { id, message }
    }

    /// Take the next [`BundleTransferId::Message`] or
    /// [`BundleTransferId::Bare`] counter value.
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

    /// Maximum content size for a Bundle message that fits in one PDU.
    fn max_single_bundle_content(&self) -> usize {
        self.pdu_size.get().saturating_sub(HEADER_SIZE)
    }

    /// Maximum segment data bytes per Transfer Segment/End message.
    ///
    /// Each segment message has: header (4) + transfer_number (4) +
    /// segment_index (4) = 12 bytes of overhead (ignoring hints on
    /// non-first segments).
    fn max_segment_data(&self) -> usize {
        self.pdu_size.get().saturating_sub(HEADER_SIZE + 8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            hints: vec![HintItem::Unknown {
                hint_type: 0x40,
                value: Bytes::from_static(&[1]),
            }],
        };
        assert_eq!(
            s.enqueue(bare.clone(), SendOptions::default()),
            Ok(BundleTransferId::Bare(u32::MAX))
        );
        assert_eq!(
            s.enqueue(bare.clone(), hinted),
            Ok(BundleTransferId::Message(0))
        );
        assert_eq!(
            s.enqueue(bare, SendOptions::default()),
            Ok(BundleTransferId::Bare(1))
        );
    }
}
