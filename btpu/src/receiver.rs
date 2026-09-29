//! The receiving end: decodes PDUs, reassembles transfers within the
//! Section 5 window under configured memory limits, and reports every
//! outcome as a [`ReceiverEvent`].

use alloc::{
    boxed::Box,
    collections::{BTreeMap, btree_map::Entry},
    vec::Vec,
};
use core::{
    fmt,
    num::{NonZeroU32, NonZeroUsize},
};

use bytes::{BufMut, Bytes, BytesMut};

use crate::{
    codec::{
        BundleExtent, DecodeOptions, Error, decode_pdu_with,
        header::MAX_CONTENT_LENGTH,
        hint::{HINT_TYPE_COUNT, HintItem, HintType, HintValue, Hints, MAX_HINT_VALUE_LEN},
        message::{Message, SEGMENT_FIELDS_SIZE, SEGMENT_FRAMING, TransferSegmentMessage},
    },
    transfer::{Admission, TransferWindow, WindowKey, WindowSize},
};

/// Bookkeeping bytes charged for every stored segment, in addition to the
/// segment's own length, when no [`MaxSegments`] limit is configured.
///
/// It is an estimate of what a stored segment costs beyond its data (the
/// [`Bytes`] handle and its map entry), not an exact heap figure, so that a
/// peer sending many tiny segments cannot hold far more memory than the
/// [`MaxTransferSize`] suggests.  It is rounded down: measured on a 64-bit
/// host, a tiny stored segment costs 70 to 100 bytes after allocator
/// rounding, so a flood can hold up to about half as much again as the
/// charged figure.  The charge counts against a transfer's
/// bookkeeping budget, not against the bundle-size cap itself (see
/// [`MaxTransferSize`]); a flood of empty or one-byte segments is bounded to
/// roughly `max_transfer_size / SEGMENT_OVERHEAD` entries.  The same bound
/// applies to an honest sender, so a link whose segments carry fewer than
/// about `SEGMENT_OVERHEAD` data bytes each needs a [`MaxSegments`] limit
/// to deliver bundles near the cap.
pub const SEGMENT_OVERHEAD: usize = 64;

/// The least bookkeeping budget a transfer has, whatever its
/// [`MaxTransferSize`]: room for 64 stored segments, or the equivalent in
/// hint bytes.
///
/// The budget is otherwise the cap itself, which for a cap of a few hundred
/// bytes would leave room for no segments at all; the floor keeps a small
/// cap from refusing every segmented transfer while still bounding what a
/// peer can hold.
pub const MIN_OVERHEAD_BUDGET: usize = 64 * SEGMENT_OVERHEAD;

/// The least limit [`MaxSegments::for_link_pdu_size`] yields, whatever the
/// cap: the same 64 segments [`MIN_OVERHEAD_BUDGET`] allows.
const MIN_SEGMENT_ALLOWANCE: MaxSegments =
    MaxSegments(NonZeroU32::new((MIN_OVERHEAD_BUDGET / SEGMENT_OVERHEAD) as u32).unwrap());

/// How many times the reference segment count
/// [`MaxSegments::for_link_pdu_size`] allows.
///
/// Headroom for segments that do not fill their PDU: the first segment's
/// hints, packing remainders, and a sender interleaving transfers within a
/// PDU (Section 4.1), where two-way interleaving alone halves every
/// segment.
const SEGMENT_ALLOWANCE_MULTIPLIER: u32 = 4;

/// What a retained hint item counts against the limits beyond its value
/// bytes: its entry in its transfer's sorted list.
///
/// Charged the same way and to the same bookkeeping budget as
/// [`SEGMENT_OVERHEAD`], except for a well-formed Bundle Length hint, which
/// is held inline and charged nothing.  The figure is the entry's size, so
/// it differs between 32- and 64-bit targets.
pub const HINT_OVERHEAD: usize = size_of::<(HintType, HintValue)>();

/// The most hint charge one transfer can hold: one item of the longest
/// value for every hint type.
const MAX_HINT_CHARGE: usize = HINT_TYPE_COUNT * (HINT_OVERHEAD + MAX_HINT_VALUE_LEN);

/// The most segment data one message can carry, hints aside.
const MAX_SEGMENT_DATA: usize = MAX_CONTENT_LENGTH - SEGMENT_FIELDS_SIZE;

/// A validated cap, in bytes (non-zero), on what a receiver accepts in one
/// transfer or unsegmented message.
///
/// The cap is a policy on the bundle's true length and a budget on the
/// state a transfer may hold, applied as data arrives.  They are separate
/// acceptance conditions: a bundle within the cap can still be refused for
/// arriving in too many segments.
///
/// - A transfer is rejected as [`RejectReason::TooLarge`] as soon as the
///   segment bytes received so far exceed the cap, or earlier if the
///   sender's Bundle Length hint promises a bundle above it.
/// - A transfer is rejected as [`RejectReason::TooFragmented`] when it holds
///   more segments, or more hint data, than the bundle it could be carrying
///   justifies.  With a [`MaxSegments`] limit configured, the number of
///   distinct segments is limited to it.  Without one, each stored
///   segment is charged [`SEGMENT_OVERHEAD`] instead.  Either way every
///   retained hint is charged [`HINT_OVERHEAD`] plus its value, except a
///   well-formed Bundle Length, which the transfer holds inline and is
///   charged nothing.  The charges count against a bookkeeping budget of the cap,
///   or [`MIN_OVERHEAD_BUDGET`] if the cap is smaller.
/// - An unsegmented Bundle message is compared against the cap by its
///   length alone, since it is never stored.
///
/// When both conditions fail on the same message, the transfer is reported
/// as `TooLarge`.  Construct via [`MaxTransferSize::new`], [`TryFrom<usize>`],
/// or [`From<NonZeroUsize>`], which enforce the bound at the edge; every
/// consumer of a `MaxTransferSize` can then rely on it.
/// There is no "unlimited" value: a receiver reassembles bundles in
/// memory, so an unbounded cap hands a remote peer a memory-exhaustion
/// lever.  Derive the cap from the deployment, the largest bundle the node
/// is meant to accept.  At or near [`Self::MAX`] the size gates stop
/// working: the receiver's counters saturate at `usize::MAX`, so neither
/// [`RejectReason::TooLarge`] nor, under the default [`MaxRetainedBytes`],
/// [`RejectReason::ReceiverFull`] can fire, and on a 32-bit target memory
/// runs out first.  The cap is per transfer;
/// see [`Receiver`] for the bound on the receiver as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct MaxTransferSize(NonZeroUsize);

impl MaxTransferSize {
    /// What the value configures, as error messages name it.
    const NAME: &str = "max transfer size";

    /// The smallest cap, one byte.
    pub const MIN: Self = Self(NonZeroUsize::MIN);

    /// The largest cap, `usize::MAX` bytes.
    pub const MAX: Self = Self(NonZeroUsize::MAX);

    /// 1 GiB, matching the Hardy TCPCLv4 transfer-MRU default.
    pub const DEFAULT: Self = Self(NonZeroUsize::new(0x4000_0000).unwrap());

    /// Returns the cap for `bytes`, or `None` if it is zero.
    pub const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Returns the cap as a plain integer.
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for MaxTransferSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

config_newtype!(MaxTransferSize: usize, NonZeroUsize);

/// A validated limit on the distinct segments one transfer may hold
/// (non-zero).
///
/// A transfer that receives a new segment with the limit already reached is
/// rejected as [`RejectReason::TooFragmented`]; repeats of a stored segment
/// never count.  The limit replaces the [`SEGMENT_OVERHEAD`] charge, so a
/// link whose segments carry few data bytes can deliver bundles up to the
/// [`MaxTransferSize`], which that charge would refuse.
///
/// Each stored segment, empty ones included, costs up to about 100 bytes of
/// heap beyond its data (see [`SEGMENT_OVERHEAD`]), so a limit of `N` lets
/// one transfer hold about `N × 100` bytes of bookkeeping, up to
/// `window_size` transfers at once, all within the [`MaxRetainedBytes`].
/// Size it for the link rather than setting it large: a peer can fill it
/// with empty segments.
///
/// [`Self::for_link_pdu_size`] derives a limit from the link's PDU size and
/// the cap; [`MaxSegments::new`], [`TryFrom<u32>`], and
/// [`From<NonZeroU32>`] take a value directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "u32", into = "u32")
)]
pub struct MaxSegments(NonZeroU32);

impl MaxSegments {
    /// What the value configures, as error messages name it.
    const NAME: &str = "max segments per transfer";

    /// The smallest limit, one segment.
    pub const MIN: Self = Self(NonZeroU32::MIN);

    /// The largest limit, `u32::MAX` segments.
    pub const MAX: Self = Self(NonZeroU32::MAX);

    /// Returns the limit for `segments`, or `None` if it is zero.
    pub const fn new(segments: u32) -> Option<Self> {
        match NonZeroU32::new(segments) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Returns the limit as a plain integer.
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// The limit for a link that delivers PDUs of at most `link_pdu_size`
    /// bytes, lower-layer headers removed, to a receiver with
    /// `max_transfer_size`.
    ///
    /// With `S` the segment data one such PDU carries (the PDU less 12 bytes
    /// of message header, transfer number, and segment index; at least 1, at
    /// most the 20-bit content-length ceiling), the limit is four times
    /// `ceil(max_transfer_size / S)`, never less than 64 and never more than
    /// `u32::MAX`.  Hints are ignored in `S`; the factor of four absorbs
    /// them along with packing remainders and a sender interleaving
    /// transfers within a PDU.  For a 1 MiB cap and 1036-byte PDUs (`S` =
    /// 1024) the limit is 4096 segments.
    ///
    /// The result is only as good as the reference: a sender whose PDUs are
    /// much smaller, that interleaves more than a few transfers per PDU, or
    /// that repeats large hints on every segment can have a valid transfer
    /// rejected.  A small `link_pdu_size` with a large cap gives a large
    /// limit; check the result against the memory budget above.
    pub fn for_link_pdu_size(link_pdu_size: usize, max_transfer_size: MaxTransferSize) -> Self {
        let segment_data = link_pdu_size
            .saturating_sub(SEGMENT_FRAMING)
            .clamp(1, MAX_SEGMENT_DATA);
        let reference =
            u32::try_from(max_transfer_size.get().div_ceil(segment_data)).unwrap_or(u32::MAX);
        let limit = reference.saturating_mul(SEGMENT_ALLOWANCE_MULTIPLIER);
        Self::new(limit).map_or(MIN_SEGMENT_ALLOWANCE, |l| l.max(MIN_SEGMENT_ALLOWANCE))
    }
}

config_newtype!(MaxSegments: u32, NonZeroU32);

/// A validated limit on the state a [`Receiver`] retains across all of its
/// in-progress transfers, in bytes (non-zero).
///
/// The retained state is what each transfer is charged: the memory its
/// stored segments keep alive, [`SEGMENT_OVERHEAD`] per stored segment, and
/// its retained hints as [`MaxTransferSize`] charges them.  A segment copied
/// out of its PDU keeps alive its own length; one kept as a view keeps
/// alive the whole PDU, at most twice its length (see [`Receiver`]), and is
/// charged that.  Segments are charged here whether or not a [`MaxSegments`]
/// limit replaces the per-transfer segment charge, so empty segments count
/// against the receiver's total too.  An FEC transfer stores no payload
/// while no FEC scheme is implemented, so it is charged only its hints;
/// the number of such transfers is bounded by the window size.
///
/// A message that would take the total over the limit rejects the transfer
/// it belongs to as [`RejectReason::ReceiverFull`], and the transfer stays
/// closed when space is freed later; transfers already held are kept.  A
/// transfer's charge is released when it is delivered, cancelled,
/// rejected, or expired.
///
/// The configured value is enforced as given.  Below `for_transfers(1, ..)`
/// (see [`Self::for_transfers`]), the most one transfer can be charged, a
/// transfer the per-transfer limits admit can be rejected as
/// [`RejectReason::ReceiverFull`] even when nothing else is held; see
/// [`ReceiverConfig`] for sizing.
///
/// The limit is on charged state, which estimates heap rather than
/// measuring it: allocator rounding adds to each entry (see
/// [`SEGMENT_OVERHEAD`]).
///
/// Construct via [`MaxRetainedBytes::new`], [`TryFrom<usize>`], or
/// [`From<NonZeroUsize>`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct MaxRetainedBytes(NonZeroUsize);

impl MaxRetainedBytes {
    /// What the value configures, as error messages name it.
    const NAME: &str = "max retained bytes";

    /// The smallest limit, one byte.
    pub const MIN: Self = Self(NonZeroUsize::MIN);

    /// The largest limit, `usize::MAX` bytes.
    pub const MAX: Self = Self(NonZeroUsize::MAX);

    /// Returns the limit for `bytes`, or `None` if it is zero.
    pub const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Returns the limit as a plain integer.
    pub const fn get(self) -> usize {
        self.0.get()
    }

    /// The limit that holds `transfers` transfers at once however their
    /// sender segments them: `transfers` times the most one transfer under
    /// `max_transfer_size` and `max_segments` may be charged, saturating at
    /// `usize::MAX`.  With `transfers` of one it is the default for
    /// [`ReceiverConfig::max_retained_bytes`].
    ///
    /// A transfer's segments hold at most `max_transfer_size` bytes of data
    /// and keep alive at most twice that, the PDUs of segments kept as
    /// views.  Without a segment limit, one transfer's most is that twice
    /// the cap plus the bookkeeping budget its segments and hints share (the
    /// cap again, or [`MIN_OVERHEAD_BUDGET`] if the cap is smaller): three
    /// times the cap for caps of at least 4 KiB, so 3 GiB for the default
    /// 1 GiB cap.  With one, it is twice the cap, plus [`SEGMENT_OVERHEAD`]
    /// for each segment the limit allows, plus the most hint charge a
    /// transfer can retain (about 37 KiB on a 64-bit target, or the
    /// bookkeeping budget if that is smaller).
    ///
    /// This is what a peer that segments as finely as the limits allow can
    /// make a transfer cost, not what a typical one does; see
    /// [`ReceiverConfig`] for the difference.
    pub const fn for_transfers(
        transfers: NonZeroUsize,
        max_transfer_size: MaxTransferSize,
        max_segments: Option<MaxSegments>,
    ) -> Self {
        const TWO: NonZeroUsize = NonZeroUsize::new(2).unwrap();
        let cap = max_transfer_size.0;
        let budget = overhead_budget(cap.get());
        let bookkeeping = match max_segments {
            None => budget,
            Some(limit) => {
                let hints = if budget < MAX_HINT_CHARGE {
                    budget
                } else {
                    MAX_HINT_CHARGE
                };
                (limit.get() as usize)
                    .saturating_mul(SEGMENT_OVERHEAD)
                    .saturating_add(hints)
            }
        };
        let held = cap.saturating_mul(TWO);
        Self(held.saturating_add(bookkeeping).saturating_mul(transfers))
    }
}

config_newtype!(MaxRetainedBytes: usize, NonZeroUsize);

/// The bookkeeping budget of one transfer under a cap of `max_transfer_size`
/// bytes: the cap itself, or [`MIN_OVERHEAD_BUDGET`] if the cap is smaller.
const fn overhead_budget(max_transfer_size: usize) -> usize {
    if max_transfer_size < MIN_OVERHEAD_BUDGET {
        MIN_OVERHEAD_BUDGET
    } else {
        max_transfer_size
    }
}

/// Configuration for a [`Receiver`].  Every field has a default, so a
/// configuration file may set only what it changes.
///
/// # Sizing
///
/// Three fields set the receiver's memory, each from something the
/// operator knows: [`max_transfer_size`](Self::max_transfer_size) from the
/// largest bundle expected,
/// [`max_segments_per_transfer`](Self::max_segments_per_transfer) from the
/// link's PDU size, and [`max_retained_bytes`](Self::max_retained_bytes)
/// from how many transfers should be held at once.  Set them in that
/// order, deriving the second with [`MaxSegments::for_link_pdu_size`] and
/// the third with [`MaxRetainedBytes::for_transfers`].
///
/// On a lossy link carrying segmented traffic, hold the window.  A
/// transfer that lost a segment stays held until the window expires it, so
/// under the default limit of one transfer's worth, a single damaged
/// transfer gets the next ones refused as [`RejectReason::ReceiverFull`]
/// until it expires.  `for_transfers` with the window size avoids that,
/// and costs little when the cap is small: 432 kB for a window of 16 and a
/// 9000-byte cap.
///
/// The bundle size alone does not fix what a transfer costs.  Each stored
/// segment is charged [`SEGMENT_OVERHEAD`] on top of the memory it keeps
/// alive, and the sender, not the receiver, decides how many segments a
/// bundle is cut into.  A sender that fills PDUs of `P` bytes, each
/// carrying `S = P - 12` data bytes after the framing, is charged at most
/// `ceil((max_transfer_size + 10) / S) * (P + 64)`: every segment keeps its
/// PDU alive, and the 10 bytes allow for the Bundle Length hint this
/// crate's sender puts on the first segment.  One that segments as finely
/// as the segment limit allows, by design or not, is charged what
/// `for_transfers` returns for one transfer.  For the default 1 GiB cap:
///
/// | Link PDU | Segment limit | Filled PDUs | Finest segmentation |
/// |----------|---------------|-------------|---------------------|
/// | 1500 bytes | 2,886,404 | 1.05 GiB | 2.17 GiB |
/// | 64 bytes | 82,595,528 | 2.46 GiB | 6.92 GiB |
///
/// Without a segment limit, the finest segmentation is charged three times
/// the cap, and a link whose PDUs carry fewer than 64 data bytes cannot
/// deliver a cap-sized bundle at all (see [`SEGMENT_OVERHEAD`]).
///
/// A retention limit at or above the first figure and below the second,
/// times the transfers to be held, admits senders that fill their PDUs and
/// refuses the finest segmentation as [`RejectReason::ReceiverFull`].  The figures are
/// charged state, not heap (see [`MaxRetainedBytes`]).
/// [`Receiver::retained_bytes`] reports what is
/// held, so a CLA can measure real traffic against these bounds.
///
/// ```
/// use core::num::NonZeroUsize;
///
/// use hardy_btpu::receiver::{MaxTransferSize, MaxRetainedBytes, MaxSegments, ReceiverConfig};
///
/// let cap = MaxTransferSize::DEFAULT;
/// let segments = MaxSegments::for_link_pdu_size(1500, cap);
/// let transfers = NonZeroUsize::new(2).unwrap();
/// let config = ReceiverConfig {
///     max_transfer_size: cap,
///     max_segments_per_transfer: Some(segments),
///     max_retained_bytes: Some(MaxRetainedBytes::for_transfers(transfers, cap, Some(segments))),
///     ..ReceiverConfig::default()
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(default, rename_all = "kebab-case")
)]
pub struct ReceiverConfig {
    /// The Section 5 transfer window.  It MUST NOT be smaller than the
    /// sender's window (Section 5); configuring the same size at both ends
    /// is RECOMMENDED.  Default: 16.
    pub window_size: WindowSize,
    /// The per-transfer reassembly cap.  Default: 1 GiB.
    pub max_transfer_size: MaxTransferSize,
    /// The most distinct segments one transfer may hold (see
    /// [`MaxSegments`], and [`MaxSegments::for_link_pdu_size`] to derive it
    /// from the link).  Default: `None`, which charges each segment
    /// [`SEGMENT_OVERHEAD`] against the bookkeeping budget instead.
    pub max_segments_per_transfer: Option<MaxSegments>,
    /// The most state the receiver retains across all in-progress
    /// transfers (see [`MaxRetainedBytes`]).  Default: `None`, which
    /// enforces [`MaxRetainedBytes::for_transfers`] one transfer at the
    /// `max_transfer_size` and `max_segments_per_transfer`, not an unlimited
    /// total.  That default grows with the segment limit: 3 GiB for the
    /// defaults, and the "Finest segmentation" figures of the sizing table
    /// above with a limit derived from the link.
    /// [`Receiver::max_retained_bytes`] reports the value in effect.
    pub max_retained_bytes: Option<MaxRetainedBytes>,
    /// Interpret the four provisional FEC message types (see
    /// [`DecodeOptions::fec`]).  Default: `false`, so they are left to relay
    /// as unknown messages.
    pub fec: bool,
}

/// Why an otherwise well-formed message was not applied to a transfer.
///
/// Deliberately exhaustive: values are produced by this crate, never decoded
/// from the wire, and a consumer that acts per-variant should get a compile
/// error when one is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The transfer was previously cancelled by the sender (Section 4.2).
    Cancelled,
    /// The transfer number is outside the current receive window (Section 5).
    OutsideWindow,
    /// A Cancel referenced a transfer that is not in progress (Section 8.4).
    UnknownTransfer,
    /// The transfer already completed and its bundle was delivered; a
    /// message for it now is a repeat (Section 6) and must not re-open it.
    Delivered,
    /// The receiver rejected the transfer earlier, for the reason carried,
    /// and reported it then as [`ReceiverEvent::TransferRejected`].
    Rejected(RejectReason),
    /// The message's segment index contradicts the transfer's established
    /// segment sequence (Section 4: segments run 0..=N with exactly one
    /// final index): a second End disagreeing with the recorded final index,
    /// an End claiming a final index below a segment already seen, or a
    /// segment beyond the final index.  Applying it would make completion
    /// permanently unsatisfiable.
    SegmentIndexConflict,
    /// The transfer is in progress and already holds a segment at this
    /// index: a repeat (Section 6).  The first copy is kept, and the
    /// repeat's hints are not applied.  A repeat after the transfer closed
    /// is reported with the reason it closed instead.
    Duplicate,
}

/// Why the receiver rejected an in-progress transfer, reported by
/// [`ReceiverEvent::TransferRejected`] and then carried by
/// [`DropReason::Rejected`] for the transfer's later messages.
///
/// Deliberately exhaustive, as [`DropReason`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The transfer's bundle exceeds the [`MaxTransferSize`]: the segment
    /// bytes stored, or the sender's Bundle Length hint, are above the cap.
    TooLarge,
    /// The transfer holds far more segments, or more hint data, than the
    /// bundle it could be carrying justifies: it exceeded its
    /// [`MaxSegments`] limit, or its bookkeeping ([`SEGMENT_OVERHEAD`] per
    /// stored segment when no limit is configured, plus retained hint
    /// bytes) exceeds the [`MaxTransferSize`] on its own.
    TooFragmented,
    /// The transfer mixed core Transfer Segment or End messages with FEC
    /// messages, which the FEC extension forbids (Section 3.2 of
    /// draft-ietf-dtn-btpu-fec); the receiver treats the transfer as
    /// cancelled.
    FecCoreMixing,
    /// An FEC transfer's FEC configuration changed mid-transfer: a different
    /// pre-agreed FEC Instance ID, a different explicit FEC Encoding ID, or a
    /// switch between the pre-agreed and explicit forms (Sections 3 and 3.1
    /// of draft-ietf-dtn-btpu-fec).  Counting a form switch as a change is
    /// this crate's reading: without the instance table the two forms
    /// cannot be shown to name the same configuration.  Scheme-specific
    /// information is not compared.  The receiver treats the transfer as
    /// cancelled.
    FecConfigurationChanged,
    /// Every segment of the transfer arrived but none held data.  Zero bytes
    /// cannot be a valid bundle: an empty Bundle Message is rejected under
    /// Section 8.1, and the same policy applies to a reassembled transfer.
    Empty,
    /// The message would have taken the state the receiver retains across
    /// all transfers over its [`MaxRetainedBytes`].  The transfer the
    /// message belongs to is the one refused; transfers already held are
    /// kept.  Like every rejection it closes the transfer for the life of
    /// the window, even if space is freed later: its stored segments are
    /// discarded, segments sent meanwhile may have been missed, and a
    /// re-opened transfer could not complete without them.
    ReceiverFull,
}

impl From<RejectReason> for DropReason {
    fn from(reason: RejectReason) -> Self {
        Self::Rejected(reason)
    }
}

/// Events emitted by the receiver for the calling CLA to act on.
///
/// Deliberately exhaustive, as [`DropReason`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverEvent {
    /// A complete bundle has been reassembled, or received whole as a Bundle
    /// message or an encapsulated bundle.
    Received {
        /// The bundle bytes.
        data: Bytes,
        /// The transfer's hint items (Section 7.2), including hint types
        /// this implementation does not recognise, so extension metadata (a
        /// correlator, say) reaches the caller without an API change.  One
        /// item per hint type: hints are transfer-scoped and repeatable,
        /// and the value from the most recently received message carrying
        /// a type supersedes any earlier one, whether across the messages
        /// of a transfer or within a single Bundle message.  A Bundle
        /// Length hint on a Bundle message is omitted (Section 9.1:
        /// receivers SHOULD ignore it there).
        ///
        /// A transfer's Bundle Length hint is passed on as the sender wrote
        /// it.  It is advisory: the receiver uses it only to reject an
        /// oversized transfer early, and it can disagree with `data`,
        /// whose length is the authoritative one.
        hints: Hints,
    },

    /// A transfer was cancelled by the sender.
    ///
    /// Also reported for a Cancel of an in-window number the receiver has
    /// seen nothing of: Section 5 counts every number in the window as in
    /// progress, and Section 8.4 has segments arriving after the Cancel
    /// discarded, so the number is remembered as cancelled either way.
    TransferCancelled {
        /// The cancelled transfer.
        transfer_number: u32,
    },

    /// A transfer was evicted from the window (incomplete).  One window
    /// advance can evict several; they are reported oldest first.
    TransferExpired {
        /// The expired transfer.
        transfer_number: u32,
    },

    /// A message was dropped without being applied to any transfer.
    /// Informational: the caller decides whether this matters (statistics,
    /// logging, or nothing at all).
    MessageDropped {
        /// The transfer the message named.
        transfer_number: u32,
        /// Why the message was dropped.
        reason: DropReason,
    },

    /// An in-progress transfer was rejected and its state discarded; later
    /// messages for it are dropped as [`DropReason::Rejected`] with the
    /// same reason.  Distinct from
    /// [`Self::TransferCancelled`], which reports a sender's Transfer Cancel,
    /// although a transfer rejected for a protocol violation is one the
    /// drafts call cancelled.
    TransferRejected {
        /// The rejected transfer.
        transfer_number: u32,
        /// Why it was rejected.
        reason: RejectReason,
    },

    /// An unsegmented Bundle message was rejected by local policy: its
    /// content exceeds the configured [`MaxTransferSize`], or it is empty
    /// (`len == 0`), which cannot be the valid bundle Section 8.1 requires.
    /// The counterpart of [`Self::TransferRejected`] for bundles that never
    /// had a transfer number.
    BundleRejected {
        /// The rejected content length in bytes.
        len: usize,
    },

    /// One message could not be decoded and was skipped; processing
    /// continued at the next message boundary given by the Section 7 header
    /// length (the skip-and-continue rule of Section 7.3).
    MalformedMessage {
        /// Why the message could not be decoded.
        error: Error,
    },

    /// The PDU could not be walked further (no message boundary could be
    /// determined, or an encapsulated bundle of unknown extent was reached,
    /// Section 7.3) and the remainder was discarded.  Always the final event
    /// of its PDU.
    ///
    /// Positions in `error` count from the start of the PDU passed to
    /// [`Receiver::receive_pdu`] or [`Receiver::receive_pdu_into`]; a caller
    /// that wants to inspect the discarded bytes keeps a clone of that
    /// `Bytes` (a reference-count increment, not a copy).
    MalformedPdu {
        /// Why the walk stopped.
        error: Error,
    },
}

/// Whether a transfer uses core segmentation or FEC, and for FEC the
/// configuration its first message named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferKind {
    Core,
    Fec(FecConfig),
}

/// The part of an FEC transfer's configuration visible without an FEC
/// scheme: which message form it uses and the identifier that form carries.
/// A pre-agreed FEC Instance ID and an explicit FEC Encoding ID name
/// different things, so the two never compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FecConfig {
    PreAgreed { instance_id: u8 },
    Explicit { encoding_id: u8 },
}

/// The content of a Transfer Segment or Transfer End, annotated with what
/// the receiver needs to store it.
struct CoreSegment {
    index: u32,
    data: Bytes,
    /// A Transfer End, whose index is the transfer's final index `N`.
    end: bool,
    /// The length of the PDU `data` was decoded from, if any, for the copy
    /// rule in [`retain_segment`].
    pdu_len: Option<usize>,
}

impl CoreSegment {
    /// Whether the segment contradicts the sequence the transfer has
    /// already established (Section 4: one transfer is segments `0..=N`).
    /// Such a segment would make completion unsatisfiable forever.
    fn conflicts(&self, transfer: &InProgressTransfer) -> bool {
        if self.end {
            // One transfer has exactly one final segment: a second End
            // disagreeing with the recorded final index, or an End claiming
            // a final index below a segment already seen.  A repeated
            // identical End is normal repetition and stays idempotent.
            transfer
                .final_segment_index
                .is_some_and(|n| n != self.index)
                || transfer
                    .segments
                    .last_key_value()
                    .is_some_and(|(&highest, _)| highest > self.index)
        } else {
            // A segment beyond the established final index would leave the
            // map's highest key above N.
            transfer.final_segment_index.is_some_and(|n| self.index > n)
        }
    }

    /// Whether the segment adds nothing to the transfer: its index is
    /// already stored and, for an End, already recorded as the final
    /// index.  An End at the index of a stored Segment still records `N`.
    fn is_duplicate(&self, transfer: &InProgressTransfer) -> bool {
        transfer.segments.contains_key(&self.index)
            && (!self.end || transfer.final_segment_index == Some(self.index))
    }
}

/// What a transfer's retained hints count against the limits:
/// [`HINT_OVERHEAD`] plus the value length for each item held as
/// [`HintItem::Unknown`].  A well-formed Bundle Length is held inline and
/// charged nothing.
fn hint_charge(hints: &Hints) -> usize {
    hints
        .unknown_values()
        .map(|value| HINT_OVERHEAD + value.len())
        .sum()
}

/// Detach a hint item's value from the PDU it was decoded from.
///
/// Hints are retained for the life of a transfer, and an unknown hint's
/// value is a view into its PDU; copying the value (at most 255 bytes) means
/// a retained hint never pins a whole PDU.
fn own_hint(hint: HintItem) -> HintItem {
    match hint {
        HintItem::Unknown { hint_type, value } => HintItem::Unknown {
            hint_type,
            value: value.detached(),
        },
        other => other,
    }
}

/// Detach a segment from its PDU when keeping the view would pin far more
/// memory than the segment is worth, and return the segment with the
/// memory it keeps alive.
///
/// A stored segment that is a view into its PDU keeps the whole PDU
/// allocation alive.  Segments of at least half the PDU are kept as views
/// and keep alive the PDU, at most twice their length; shorter ones are
/// copied and keep alive only themselves.  A message not decoded from a PDU
/// (`pdu_len` is `None`) is stored as given and counted at its own length.
fn retain_segment(data: Bytes, pdu_len: Option<usize>) -> (Bytes, usize) {
    match pdu_len {
        Some(pdu_len) if data.len() < pdu_len.div_ceil(2) => {
            let len = data.len();
            (Bytes::copy_from_slice(&data), len)
        }
        Some(pdu_len) => (data, pdu_len),
        None => {
            let len = data.len();
            (data, len)
        }
    }
}

struct InProgressTransfer {
    kind: TransferKind,
    segments: BTreeMap<u32, Bytes>,
    final_segment_index: Option<u32>,
    hints: Hints,
    /// Segment bytes received so far, including a segment refused for
    /// exceeding the segment limit: the bundle's length as far as it is
    /// known, compared against the [`MaxTransferSize`].
    data_bytes: usize,
    /// The memory the stored segments keep alive, as [`retain_segment`]
    /// reported it for each.
    held_bytes: usize,
    /// A new segment arrived with the segment limit already reached, and
    /// was not stored.
    over_segment_limit: bool,
}

impl InProgressTransfer {
    fn new(kind: TransferKind) -> Self {
        Self {
            kind,
            segments: BTreeMap::new(),
            final_segment_index: None,
            hints: Hints::new(),
            data_bytes: 0,
            held_bytes: 0,
            over_segment_limit: false,
        }
    }

    /// Insert a segment unless its index is already stored, detaching it
    /// from its PDU per [`retain_segment`].  Repeats are dropped before
    /// this; an occupied index here is an End naming a stored Segment's
    /// index as final, which keeps the stored copy.
    ///
    /// With a `segment_limit`, a new segment that would exceed it is not
    /// stored but its bytes are still counted, so [`Self::exceeds`] can
    /// prefer [`RejectReason::TooLarge`] when the bundle is over both limits.
    fn insert_segment(
        &mut self,
        index: u32,
        data: Bytes,
        pdu_len: Option<usize>,
        segment_limit: Option<u64>,
    ) {
        // usize is at most 64 bits on every supported target.
        let stored = self.segments.len() as u64;
        let Entry::Vacant(e) = self.segments.entry(index) else {
            return;
        };
        self.data_bytes = self.data_bytes.saturating_add(data.len());
        if segment_limit.is_some_and(|limit| stored >= limit) {
            self.over_segment_limit = true;
            return;
        }
        let (data, held) = retain_segment(data, pdu_len);
        self.held_bytes = self.held_bytes.saturating_add(held);
        e.insert(data);
    }

    /// Which draft-ietf-dtn-btpu-fec rule a message of `kind` breaks on
    /// this transfer, if any: mixing core and FEC messages (Section 3.2) is
    /// [`RejectReason::FecCoreMixing`], and changing the FEC configuration
    /// mid-transfer (Sections 3 and 3.1) is
    /// [`RejectReason::FecConfigurationChanged`].  Either MUST cancel the
    /// transfer.
    fn kind_mismatch(&self, kind: TransferKind) -> Option<RejectReason> {
        match (self.kind, kind) {
            (established, kind) if established == kind => None,
            (TransferKind::Fec(_), TransferKind::Fec(_)) => {
                Some(RejectReason::FecConfigurationChanged)
            }
            _ => Some(RejectReason::FecCoreMixing),
        }
    }

    /// Which [`MaxTransferSize`] rule the transfer provably breaks, if any:
    /// [`RejectReason::TooLarge`] when the bytes received or the sender's
    /// Bundle Length hint exceed `max`, [`RejectReason::TooFragmented`] when a
    /// segment was refused by the segment limit or the bookkeeping exceeds
    /// its budget.  The bookkeeping is the hint charge, plus
    /// [`SEGMENT_OVERHEAD`] per stored segment when there is no
    /// `segment_limit`.
    fn exceeds(&self, max: usize, segment_limit: Option<u64>) -> Option<RejectReason> {
        let segments = match segment_limit {
            Some(_) => 0,
            None => self.segment_charge(),
        };
        if self.data_bytes > max || self.hints.bundle_length().is_some_and(|h| h > max as u64) {
            Some(RejectReason::TooLarge)
        } else if self.over_segment_limit
            || segments.saturating_add(hint_charge(&self.hints)) > overhead_budget(max)
        {
            Some(RejectReason::TooFragmented)
        } else {
            None
        }
    }

    /// [`SEGMENT_OVERHEAD`] for each stored segment.
    fn segment_charge(&self) -> usize {
        self.segments.len().saturating_mul(SEGMENT_OVERHEAD)
    }

    /// What the transfer counts against the receiver's [`MaxRetainedBytes`]:
    /// the memory its segments keep alive, [`SEGMENT_OVERHEAD`] per stored
    /// segment whatever the segment limit, and its hints.
    fn charge(&self) -> usize {
        self.held_bytes
            .saturating_add(self.segment_charge())
            .saturating_add(hint_charge(&self.hints))
    }

    /// Record hints from a message, keeping the latest value per hint type.
    fn apply_hints(&mut self, hints: Vec<HintItem>) {
        self.hints.extend(hints.into_iter().map(own_hint));
    }

    /// Check whether all segments 0..=N have been received.
    fn is_complete(&self) -> bool {
        let Some(n) = self.final_segment_index else {
            return false;
        };
        // `n + 1` distinct indices, the greatest `n`, can only be 0..=n.
        // Counted in u64: `n` is wire-supplied, so `n + 1` overflows u32
        // when a hostile End claims a final index of u32::MAX.
        self.segments.len() as u64 == u64::from(n) + 1
            && self.segments.last_key_value().map(|(k, _)| *k) == Some(n)
    }

    /// Concatenate segments in order and return the reassembled bundle.
    ///
    /// A single-segment transfer hands back its lone [`Bytes`] as stored,
    /// with no further copy: a view of its PDU if the segment was at least
    /// half the PDU, otherwise the copy [`retain_segment`] made on arrival.
    /// Multi-segment reassembly deliberately copies once
    /// into a contiguous buffer: the BPA parses bundles from contiguous
    /// bytes, and one copy per delivered bundle is cheap relative to the
    /// transfer itself.
    fn reassemble(&self) -> Bytes {
        if let Some((_, only)) = self.segments.first_key_value()
            && self.segments.len() == 1
        {
            return only.clone();
        }
        let total = self.segments.values().map(Bytes::len).sum();
        let mut buf = BytesMut::with_capacity(total);
        for data in self.segments.values() {
            buf.put_slice(data);
        }
        buf.freeze()
    }
}

/// The in-progress transfers and the sum of their charges.
///
/// The sum equals the transfers' charges by construction: every change to a
/// held transfer goes through [`Self::modify`], which re-charges it, and
/// every removal releases the charge of what it removes.
#[derive(Default)]
struct Held {
    /// Keyed in window order, oldest first, so a window advance expires a
    /// leading run of entries and costs only what it expires.
    transfers: BTreeMap<WindowKey, InProgressTransfer>,
    /// The sum of [`InProgressTransfer::charge`] over `transfers`.
    retained: usize,
}

impl Held {
    fn get(&self, key: WindowKey) -> Option<&InProgressTransfer> {
        self.transfers.get(&key)
    }

    fn len(&self) -> usize {
        self.transfers.len()
    }

    /// The newest transfer's key, if any transfer is held.
    fn newest(&self) -> Option<WindowKey> {
        self.transfers.last_key_value().map(|(&key, _)| key)
    }

    /// The transfer at `key`, opened as a new transfer of `kind` if none is
    /// held.  A new transfer holds nothing, so it is charged nothing.
    fn get_or_open(&mut self, key: WindowKey, kind: TransferKind) -> &InProgressTransfer {
        self.transfers
            .entry(key)
            .or_insert_with(|| InProgressTransfer::new(kind))
    }

    /// Apply `f` to the transfer at `key`, if one is held, and re-charge it.
    fn modify<R>(
        &mut self,
        key: WindowKey,
        f: impl FnOnce(&mut InProgressTransfer) -> R,
    ) -> Option<R> {
        let transfer = self.transfers.get_mut(&key)?;
        let before = transfer.charge();
        let result = f(transfer);
        self.retained = self
            .retained
            .saturating_sub(before)
            .saturating_add(transfer.charge());
        Some(result)
    }

    /// Remove the transfer at `key`, if one is held, releasing its charge.
    fn remove(&mut self, key: WindowKey) -> Option<InProgressTransfer> {
        let transfer = self.transfers.remove(&key)?;
        self.retained = self.retained.saturating_sub(transfer.charge());
        Some(transfer)
    }

    /// Remove the oldest transfer if `window` has moved past it, releasing
    /// its charge, and return its key.
    fn pop_expired(&mut self, window: &TransferWindow) -> Option<WindowKey> {
        let entry = self
            .transfers
            .first_entry()
            .filter(|entry| window.is_expired(*entry.key()))?;
        let (key, transfer) = entry.remove_entry();
        self.retained = self.retained.saturating_sub(transfer.charge());
        Some(key)
    }

    fn clear(&mut self) {
        self.transfers.clear();
        self.retained = 0;
    }
}

/// Manages inbound PDU processing, transfer window, and segment reassembly.
///
/// # Memory
///
/// Each in-progress transfer is bounded by the [`MaxTransferSize`]: its
/// segment bytes may not exceed the cap, and its bookkeeping (segments and
/// hints) has a budget of its own; with a [`MaxSegments`] limit the segment
/// count is limited directly instead.  A segment shorter than half its PDU
/// is copied out; a longer one stays a view that keeps alive at most twice
/// its length, and is charged that, provided each PDU arrives in a buffer
/// of its own size (a `Bytes` split from a larger receive buffer pins all
/// of it).  So a transfer is charged at most three times the cap.  Up to
/// `window_size` transfers can be in progress, charged together at most the
/// [`MaxRetainedBytes`], by default one transfer's full allowance.  Charged
/// state estimates retained heap (see [`SEGMENT_OVERHEAD`]); receive
/// buffers, returned events, and allocator overhead are separate.  Closed
/// transfer numbers are remembered until the window passes them, so
/// in-progress and closed transfers together number at most the window
/// size.
///
/// # Sender restarts
///
/// A restarted sender that begins from a random transfer number (Section 4)
/// is accepted only about half the time; see [`TransferWindow`].  A CLA
/// that learns of a restart out of band should call [`Self::reset`].
pub struct Receiver {
    state: Reassembly,
    /// Held apart from `state` so that [`Self::receive_pdu`] can lend it to
    /// the decoder while mutating `state`: the two are disjoint borrows, so
    /// the hook is neither shared nor taken out and put back (which a
    /// panicking hook would leave undone).  A `Box` rather than an `Arc`
    /// keeps the crate buildable on targets without pointer-width atomics.
    bundle_extent: Option<Box<dyn BundleExtent + Send + Sync>>,
}

/// A [`Receiver`]'s configuration and reassembly state: everything but the
/// extent hook.
struct Reassembly {
    max_transfer_size: MaxTransferSize,
    /// [`ReceiverConfig::max_segments_per_transfer`], widened once for the
    /// comparison against a segment count.
    segment_limit: Option<u64>,
    /// [`ReceiverConfig::max_retained_bytes`], or its default.
    max_retained_bytes: MaxRetainedBytes,
    fec: bool,
    window: TransferWindow,
    held: Held,
    /// In-window transfers that are over, and why: delivered, cancelled by
    /// the sender, or rejected by local policy.  A message for one of them
    /// is a repeat or a straggler and must not re-open it (Section 4.2 for
    /// cancelled transfers; the same trap for the rest).  Keys are always
    /// in-window (pruned by [`Self::expire_old_transfers`]), so the map is
    /// bounded by the window size.
    closed: BTreeMap<WindowKey, DropReason>,
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = &self.state;
        f.debug_struct("Receiver")
            .field("max_transfer_size", &state.max_transfer_size)
            .field("segment_limit", &state.segment_limit)
            .field("max_retained_bytes", &state.max_retained_bytes)
            .field("retained", &state.held.retained)
            .field("fec", &state.fec)
            .field("bundle_extent", &self.bundle_extent.is_some())
            .field("window", &state.window)
            .field("transfers", &state.held.len())
            .field("closed", &state.closed.len())
            .finish()
    }
}

impl Receiver {
    /// Create a new receiver.
    ///
    /// Transfers that provably exceed the configured [`MaxTransferSize`] or
    /// segment allowance, that break the FEC extension's rules, or that
    /// complete with no data are rejected with
    /// [`ReceiverEvent::TransferRejected`], and oversized or empty Bundle
    /// messages with [`ReceiverEvent::BundleRejected`].
    pub fn new(config: ReceiverConfig) -> Self {
        let default_retention = MaxRetainedBytes::for_transfers(
            NonZeroUsize::MIN,
            config.max_transfer_size,
            config.max_segments_per_transfer,
        );
        Self {
            state: Reassembly {
                max_transfer_size: config.max_transfer_size,
                segment_limit: config.max_segments_per_transfer.map(|m| u64::from(m.get())),
                max_retained_bytes: config.max_retained_bytes.unwrap_or(default_retention),
                fec: config.fec,
                window: TransferWindow::new(config.window_size),
                held: Held::default(),
                closed: BTreeMap::new(),
            },
            bundle_extent: None,
        }
    }

    /// Supply the caller's way of finding the extent of an encapsulated
    /// bundle (see [`BundleExtent`] and [`DecodeOptions::bundle_extent`]).
    ///
    /// Without one, a bare bundle frame is taken to be the whole PDU, link
    /// padding included, and a bundle found after a message ends the PDU
    /// with [`ReceiverEvent::MalformedPdu`]; see
    /// [`decode_pdu`](crate::codec::decode_pdu) for the padding pitfalls.
    pub fn with_bundle_extent(
        mut self,
        bundle_extent: impl BundleExtent + Send + Sync + 'static,
    ) -> Self {
        self.bundle_extent = Some(Box::new(bundle_extent));
        self
    }

    /// Discard every in-progress transfer, forget closed transfer numbers,
    /// and return the window to its initial state, as if the receiver were
    /// newly constructed.  Configuration is kept.  No events are produced:
    /// the caller knows what it discarded.
    pub fn reset(&mut self) {
        let state = &mut self.state;
        state.window.reset();
        state.held.clear();
        state.closed.clear();
    }

    /// The state held across all in-progress transfers, charged as
    /// [`MaxRetainedBytes`] counts it.
    ///
    /// Read between calls, it lets a CLA export a gauge and compare what
    /// its peers cost against the configured limit (see
    /// [`ReceiverConfig`] for sizing).
    pub fn retained_bytes(&self) -> usize {
        self.state.held.retained
    }

    /// The [`MaxRetainedBytes`] in effect: the configured value, or the
    /// default [`ReceiverConfig::max_retained_bytes`] derives from the other
    /// limits.
    pub fn max_retained_bytes(&self) -> MaxRetainedBytes {
        self.state.max_retained_bytes
    }

    /// Process a received convergence layer PDU.  Returns zero or more events.
    ///
    /// Infallible at the PDU level: every framing and semantic fault is
    /// expressed as an event ([`ReceiverEvent::MalformedMessage`],
    /// [`ReceiverEvent::MalformedPdu`], [`ReceiverEvent::MessageDropped`],
    /// ...) alongside whatever the well-formed messages produced, so a
    /// fault late in a PDU never discards the events of the prefix before
    /// it.
    ///
    /// Taking `pdu` by value (rather than `&[u8]`) lets the codec extract
    /// message data as zero-copy [`Bytes`] views into the original buffer.
    /// A `Bytes` made from a fresh `Vec` allocates its shared header the
    /// first time it is sliced; with [`Self::receive_pdu_into`] that is the
    /// only allocation a PDU of whole Bundle messages costs.
    ///
    /// The list grows with the number of messages in the PDU, which the
    /// peer chooses: most messages can produce an event, and one that
    /// advances the window also reports each transfer it expires.  A PDU
    /// of 8-byte Transfer Cancels for unknown transfers yields one
    /// [`ReceiverEvent::MessageDropped`] per 8 bytes, a list several times
    /// the PDU's size.  [`Self::receive_pdu_into`] fills a list the caller
    /// keeps instead, so a CLA reusing one list pays for that growth once
    /// rather than on every PDU.
    pub fn receive_pdu(&mut self, pdu: Bytes) -> Vec<ReceiverEvent> {
        let mut events = Vec::new();
        self.receive_pdu_into(pdu, &mut events);
        events
    }

    /// [`Self::receive_pdu`], writing the events into `events` rather than
    /// a new list.
    ///
    /// `events` is cleared first, so after the call it holds exactly this
    /// PDU's events; its capacity is kept, so a list reused across PDUs is
    /// reallocated only when a PDU produces more events than any before it.
    pub fn receive_pdu_into(&mut self, pdu: Bytes, events: &mut Vec<ReceiverEvent>) {
        events.clear();
        let pdu_len = pdu.len();
        // The decoder borrows the hook for the whole PDU while `state` is
        // mutated; the fields are disjoint, so both borrows hold at once.
        let Self {
            state,
            bundle_extent,
        } = self;
        let options = DecodeOptions {
            fec: state.fec,
            bundle_extent: bundle_extent.as_deref().map(|e| e as &dyn BundleExtent),
        };
        let mut messages = decode_pdu_with(pdu, options);
        while let Some(item) = messages.next() {
            match item {
                Ok(msg) => state.process_into(msg, Some(pdu_len), events),
                Err(error) if messages.is_exhausted() => {
                    events.push(ReceiverEvent::MalformedPdu { error });
                }
                Err(error) => {
                    events.push(ReceiverEvent::MalformedMessage { error });
                }
            }
        }
    }

    /// Process a single decoded message.
    ///
    /// A message given here is not associated with a PDU, so its segment
    /// data is stored as supplied rather than copied (see [`Receiver`] for
    /// the copy rule `receive_pdu` applies).
    pub fn process_message(&mut self, message: Message) -> Vec<ReceiverEvent> {
        let mut events = Vec::new();
        self.state.process_into(message, None, &mut events);
        events
    }
}

impl Reassembly {
    /// Process one message, appending its events to `events`.  `pdu_len` is
    /// the length of the PDU the message was decoded from, if any.
    fn process_into(
        &mut self,
        message: Message,
        pdu_len: Option<usize>,
        events: &mut Vec<ReceiverEvent>,
    ) {
        match message {
            Message::DefinitePadding { .. } | Message::Unknown { .. } => {}

            Message::Bundle { data, hints } => {
                // Section 8.1: the content MUST be a valid bundle, which zero
                // bytes cannot be; and the size cap applies unstored.
                if data.is_empty() || data.len() > self.max_transfer_size.get() {
                    events.push(ReceiverEvent::BundleRejected { len: data.len() });
                    return;
                }
                // Section 9.1: a Bundle Length hint is only meaningful on
                // Transfer Segment and End messages and SHOULD be ignored
                // elsewhere.
                let mut hints = Hints::from(hints);
                hints.remove(HintType::BUNDLE_LENGTH);
                events.push(ReceiverEvent::Received { data, hints });
            }

            Message::TransferSegment(m) => self.process_segment(m, false, pdu_len, events),
            Message::TransferEnd(m) => self.process_segment(m, true, pdu_len, events),

            Message::TransferCancel { transfer_number } => {
                self.process_transfer_cancel(transfer_number, events)
            }

            // FEC messages are tracked but not decoded (no FEC scheme is
            // implemented).  They are stored with their FecConfig to detect
            // mixing and configuration changes.
            Message::PreAgreedFecSource(m) | Message::PreAgreedFecRepair(m) => self
                .process_transfer_message(
                    m.transfer_number,
                    TransferKind::Fec(FecConfig::PreAgreed {
                        instance_id: m.fec_instance_id,
                    }),
                    m.hints,
                    None,
                    events,
                ),
            Message::ExplicitFecSource(m) | Message::ExplicitFecRepair(m) => self
                .process_transfer_message(
                    m.transfer_number,
                    TransferKind::Fec(FecConfig::Explicit {
                        encoding_id: m.fec_encoding_id,
                    }),
                    m.hints,
                    None,
                    events,
                ),
        }
    }

    /// [`Self::process_transfer_message`] for a Transfer Segment, or for a
    /// Transfer End if `end`.
    fn process_segment(
        &mut self,
        m: TransferSegmentMessage,
        end: bool,
        pdu_len: Option<usize>,
        events: &mut Vec<ReceiverEvent>,
    ) {
        let segment = CoreSegment {
            index: m.segment_index,
            data: m.data,
            end,
            pdu_len,
        };
        self.process_transfer_message(
            m.transfer_number,
            TransferKind::Core,
            m.hints,
            Some(segment),
            events,
        );
    }

    /// Admission check: is `transfer_number` eligible for processing at all?
    /// It must be inside the receive window and not already closed
    /// (delivered, cancelled, or rejected).
    ///
    /// Returns the transfer's key, or the reason to drop the message.  A
    /// number that advances the window expires the transfers it moves past,
    /// and their [`ReceiverEvent::TransferExpired`] events are pushed onto
    /// `events` whatever the outcome.
    fn admit(
        &mut self,
        transfer_number: u32,
        events: &mut Vec<ReceiverEvent>,
    ) -> Result<WindowKey, DropReason> {
        let key = match self.window.admit(transfer_number) {
            Admission::OutsideWindow => return Err(DropReason::OutsideWindow),
            Admission::New(key) => {
                self.expire_old_transfers(events);
                key
            }
            Admission::InProgress(key) => key,
        };

        // A repeated message for a closed transfer MUST NOT re-open it
        // (Section 4.2 for cancelled; the same trap applies to delivered
        // and locally rejected transfers).  Checked after the window (the
        // traffic is still window-relevant) but before any transfer entry
        // is inserted.
        match self.closed.get(&key) {
            Some(&reason) => Err(reason),
            None => Ok(key),
        }
    }

    /// Which limit, if any, a state change has taken the transfer at `key`
    /// or the receiver over: the transfer's [`MaxTransferSize`] rules (see
    /// [`InProgressTransfer::exceeds`]), then the receiver's
    /// [`MaxRetainedBytes`] as [`RejectReason::ReceiverFull`].
    ///
    /// The receiver's total was within its limit before the change, so the
    /// transfer that grew is the one to reject.
    fn over_limit(&self, key: WindowKey) -> Option<RejectReason> {
        self.held
            .get(key)
            .and_then(|t| t.exceeds(self.max_transfer_size.get(), self.segment_limit))
            .or_else(|| {
                (self.held.retained > self.max_retained_bytes.get())
                    .then_some(RejectReason::ReceiverFull)
            })
    }

    /// Report a message that is dropped with no state touched.  Drops are
    /// expected traffic (repetition, reordering, a moved window), not
    /// faults, so they surface as [`ReceiverEvent::MessageDropped`].
    fn drop_message(transfer_number: u32, reason: DropReason, events: &mut Vec<ReceiverEvent>) {
        events.push(ReceiverEvent::MessageDropped {
            transfer_number,
            reason,
        });
    }

    /// [`Self::close`] the transfer at `key` as rejected for `reason`, and
    /// report it.
    fn reject_transfer(
        &mut self,
        key: WindowKey,
        reason: RejectReason,
        events: &mut Vec<ReceiverEvent>,
    ) {
        self.close(key, reason.into());
        events.push(ReceiverEvent::TransferRejected {
            transfer_number: key.transfer_number(),
            reason,
        });
    }

    /// Shared pipeline for every message that opens or extends a transfer
    /// (Segment, End, and the four FEC messages): admission, transfer-kind
    /// check, sequence-conflict check, state application, oversize gate,
    /// completion check.
    ///
    /// `kind` is what this message would make a new transfer.  `segment` is
    /// the content of a Segment or End, and `None` for an FEC message,
    /// whose payload is not stored while no FEC scheme is implemented; its
    /// hints are still kept, and the Bundle Length hint still policed.
    fn process_transfer_message(
        &mut self,
        transfer_number: u32,
        kind: TransferKind,
        hints: Vec<HintItem>,
        segment: Option<CoreSegment>,
        events: &mut Vec<ReceiverEvent>,
    ) {
        let key = match self.admit(transfer_number, events) {
            Ok(key) => key,
            Err(reason) => return Self::drop_message(transfer_number, reason, events),
        };

        let transfer = self.held.get_or_open(key, kind);

        if let Some(reason) = transfer.kind_mismatch(kind) {
            return self.reject_transfer(key, reason, events);
        }

        // A conflicting or duplicate message is dropped with no state
        // touched, hints included.
        if segment.as_ref().is_some_and(|s| s.conflicts(transfer)) {
            return Self::drop_message(transfer_number, DropReason::SegmentIndexConflict, events);
        }
        if segment.as_ref().is_some_and(|s| s.is_duplicate(transfer)) {
            return Self::drop_message(transfer_number, DropReason::Duplicate, events);
        }

        let segment_limit = self.segment_limit;
        self.held.modify(key, |transfer| {
            transfer.apply_hints(hints);
            if let Some(CoreSegment {
                index,
                data,
                end,
                pdu_len,
            }) = segment
            {
                if end {
                    transfer.final_segment_index = Some(index);
                }
                // An empty segment or End is still stored: Section 4
                // completes a transfer once indices 0..=N are present, and a
                // streaming sender may have no other way to finish.  The
                // segment charge bounds a flood of them.
                transfer.insert_segment(index, data, pdu_len, segment_limit);
            }
        });

        if let Some(reason) = self.over_limit(key) {
            return self.reject_transfer(key, reason, events);
        }

        // A late segment may fill the last gap after the End arrived.
        self.complete_if_ready(key, events);
    }

    /// If the transfer's segments are all present (and its final index is
    /// known), reassemble it, remove it from the window, and push a
    /// `Received` event.  A no-op otherwise.  Called after every segment
    /// or End insert so out-of-order completion is detected regardless of which
    /// message arrives last.
    fn complete_if_ready(&mut self, key: WindowKey, events: &mut Vec<ReceiverEvent>) {
        let Some(transfer) = self.held.get(key) else {
            return;
        };
        if !transfer.is_complete() {
            return;
        }
        // The size limits were enforced on every insert.  Zero bytes cannot
        // be a valid bundle, as for an empty Bundle Message (Section 8.1).
        if transfer.data_bytes == 0 {
            return self.reject_transfer(key, RejectReason::Empty, events);
        }
        // Reassembled while the segments are still charged, so the copy is
        // never made against budget accounted free.
        let data = transfer.reassemble();
        // Closed rather than forgotten: a sender may repeat any message
        // (Section 6), and a repeat must not deliver the bundle again.
        let hints = self.close(key, DropReason::Delivered);
        events.push(ReceiverEvent::Received { data, hints });
    }

    fn process_transfer_cancel(&mut self, transfer_number: u32, events: &mut Vec<ReceiverEvent>) {
        // Section 8.4 ignores a Cancel for a transfer not in progress, and
        // Section 5 defines in progress by the window alone.  So a Cancel
        // never advances the window, and one inside it is recorded even
        // before any segment arrives, so later segments are discarded.
        let Some(key) = self.window.key(transfer_number) else {
            return Self::drop_message(transfer_number, DropReason::UnknownTransfer, events);
        };

        // A repeated Cancel of a transfer already closed is idempotent and
        // reported with the original reason.
        if let Some(&reason) = self.closed.get(&key) {
            return Self::drop_message(transfer_number, reason, events);
        }

        // A closed transfer is never also in progress, so this discards the
        // transfer's segments if any have arrived, and otherwise records
        // the Cancel ahead of them.
        self.close(key, DropReason::Cancelled);
        events.push(ReceiverEvent::TransferCancelled { transfer_number });
    }

    /// Drop every transfer, live or closed, that the window has moved past,
    /// reporting the live ones as [`ReceiverEvent::TransferExpired`] oldest
    /// first.  Both maps are keyed in window order, so the expired entries
    /// are a leading run and the cost is O(log n) per entry expired, not a
    /// walk of the window.
    fn expire_old_transfers(&mut self, events: &mut Vec<ReceiverEvent>) {
        debug_assert!(
            self.held
                .newest()
                .is_none_or(|k| self.window.is_behind_greatest(k))
                && self
                    .closed
                    .last_key_value()
                    .is_none_or(|(&k, _)| self.window.is_behind_greatest(k)),
            "a held transfer is ahead of the window"
        );
        while let Some(key) = self.held.pop_expired(&self.window) {
            events.push(ReceiverEvent::TransferExpired {
                transfer_number: key.transfer_number(),
            });
        }

        // Prune the closed map the same way; this is what keeps it bounded
        // by the window size.  No events: these were already reported as
        // Received / TransferCancelled / TransferRejected when they
        // closed.
        while let Some(entry) = self.closed.first_entry()
            && self.window.is_expired(*entry.key())
        {
            entry.remove();
        }

        // Every key in either map is in the window, and no key is in both.
        debug_assert!(
            self.held.len() + self.closed.len() <= usize::from(self.window.window_size().get()),
            "more transfers remembered than the window holds"
        );
    }

    /// Remember the transfer at `key` as closed with `reason`, so later
    /// messages for it are dropped rather than re-opening it, and remove its
    /// in-progress state, if any, releasing its charge.  Returns the
    /// transfer's hints, empty if it had no state.
    fn close(&mut self, key: WindowKey, reason: DropReason) -> Hints {
        self.closed.insert(key, reason);
        self.held
            .remove(key)
            .map(|transfer| transfer.hints)
            .unwrap_or_default()
    }

    /// The in-progress transfer numbered `transfer_number`, looked up
    /// through the window as production code does.
    #[cfg(test)]
    fn transfer(&self, transfer_number: u32) -> Option<&InProgressTransfer> {
        self.held.get(self.window.key(transfer_number)?)
    }

    /// Why the transfer numbered `transfer_number` closed, if it has.
    #[cfg(test)]
    fn closed_reason(&self, transfer_number: u32) -> Option<&DropReason> {
        self.closed.get(&self.window.key(transfer_number)?)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::{codec::encode_message, fec::PreAgreedFecMessage};

    fn receiver(window_size: u16, max_transfer_size: usize) -> Receiver {
        Receiver::new(ReceiverConfig {
            window_size: WindowSize::try_from(window_size).unwrap(),
            max_transfer_size: MaxTransferSize::try_from(max_transfer_size).unwrap(),
            max_segments_per_transfer: None,
            max_retained_bytes: None,
            fec: false,
        })
    }

    fn segment(transfer_number: u32, segment_index: u32, data: &'static [u8]) -> Message {
        Message::TransferSegment(TransferSegmentMessage {
            transfer_number,
            segment_index,
            hints: vec![],
            data: Bytes::from_static(data),
        })
    }

    fn end(transfer_number: u32, segment_index: u32, data: &'static [u8]) -> Message {
        Message::TransferEnd(TransferSegmentMessage {
            transfer_number,
            segment_index,
            hints: vec![],
            data: Bytes::from_static(data),
        })
    }

    #[test]
    fn closed_map_pruned_by_window_advance() {
        let mut r = receiver(4, usize::MAX);
        r.process_message(segment(0, 0, b"x"));
        r.process_message(Message::TransferCancel { transfer_number: 0 });
        assert_eq!(r.state.closed_reason(0), Some(&DropReason::Cancelled));

        // Advance the window until 0 falls out of it.
        for t in 1..=4u32 {
            r.process_message(segment(t, 0, b"x"));
        }
        assert!(r.state.closed.is_empty());

        // A really late segment for 0 is now dropped as out-of-window.
        let events = r.process_message(segment(0, 0, b"x"));
        assert_eq!(
            events,
            vec![ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::OutsideWindow,
            }]
        );
        assert!(r.state.transfer(0).is_none());
    }

    #[test]
    fn closed_map_pruned_across_the_wrap() {
        let mut r = receiver(4, usize::MAX);
        for t in [u32::MAX - 1, u32::MAX, 0] {
            r.process_message(end(t, 0, b"x"));
        }
        assert_eq!(r.state.closed.len(), 3);

        // 1 is new and leaves MAX - 1 three behind the next number; 4 moves
        // the window past everything delivered.
        r.process_message(segment(1, 0, b"x"));
        assert_eq!(r.state.closed.len(), 3);
        r.process_message(segment(4, 0, b"x"));
        assert!(r.state.closed.is_empty());
    }

    fn unknown(hint_type: u8, value: &'static [u8]) -> HintItem {
        HintItem::Unknown {
            hint_type: HintType::new(hint_type).unwrap(),
            value: HintValue::new(Bytes::from_static(value)).unwrap(),
        }
    }

    fn segment_with(
        transfer_number: u32,
        segment_index: u32,
        hints: Vec<HintItem>,
        data: &'static [u8],
    ) -> Message {
        Message::TransferSegment(TransferSegmentMessage {
            transfer_number,
            segment_index,
            hints,
            data: Bytes::from_static(data),
        })
    }

    #[test]
    fn segments_and_hints_are_charged_to_overhead_not_data() {
        let mut r = Receiver::new(ReceiverConfig::default());
        r.process_message(segment(0, 0, b"a"));
        r.process_message(segment(0, 1, b""));
        r.process_message(segment_with(
            0,
            2,
            vec![HintItem::BundleLength(3), unknown(0x41, b"corr")],
            b"b",
        ));
        let t = r.state.transfer(0).unwrap();
        assert_eq!(t.segments.len(), 3);
        assert_eq!(t.data_bytes, 2);
        assert_eq!(t.held_bytes, 2);
        // The Bundle Length is held inline and charged nothing.
        assert_eq!(t.hints.bundle_length(), Some(3));
        assert_eq!(hint_charge(&t.hints), HINT_OVERHEAD + 4);

        // Superseding a hint releases the old value's charge.
        r.process_message(segment_with(0, 3, vec![unknown(0x41, b"c")], b"c"));
        assert_eq!(
            hint_charge(&r.state.transfer(0).unwrap().hints),
            HINT_OVERHEAD + 1
        );

        // And a larger value raises it again.
        r.process_message(segment_with(0, 4, vec![unknown(0x41, b"longer")], b"d"));
        let t = r.state.transfer(0).unwrap();
        assert_eq!(hint_charge(&t.hints), HINT_OVERHEAD + 6);
        assert_eq!(
            r.retained_bytes(),
            t.held_bytes + 5 * SEGMENT_OVERHEAD + hint_charge(&t.hints)
        );
    }

    #[test]
    fn malformed_bundle_length_is_charged_and_bundle_length_is_not() {
        let hints = Hints::from(vec![HintItem::BundleLength(10), unknown(0, b"abc")]);
        assert_eq!(hint_charge(&hints), HINT_OVERHEAD + 3);
        let hints = Hints::from(vec![unknown(0, b"abc"), HintItem::BundleLength(10)]);
        assert_eq!(hint_charge(&hints), 0);
    }

    #[test]
    fn retained_segment_is_charged_what_it_keeps_alive() {
        let pdu = Bytes::from(vec![0; 100]);
        // At least half the PDU: kept as a view, charged the whole PDU.
        let (data, held) = retain_segment(pdu.slice(..50), Some(pdu.len()));
        assert_eq!(data.as_ptr(), pdu.as_ptr());
        assert_eq!(held, 100);
        // Shorter: copied, charged its own length.
        let (data, held) = retain_segment(pdu.slice(..49), Some(pdu.len()));
        assert_ne!(data.as_ptr(), pdu.as_ptr());
        assert_eq!(held, 49);
        // An odd PDU length rounds the half up, so a view never keeps alive
        // more than twice its length.
        let (data, held) = retain_segment(pdu.slice(..50), Some(101));
        assert_ne!(data.as_ptr(), pdu.as_ptr());
        assert_eq!(held, 50);
        // Not from a PDU: stored as given.
        let (data, held) = retain_segment(pdu.slice(..10), None);
        assert_eq!(data.as_ptr(), pdu.as_ptr());
        assert_eq!(held, 10);
    }

    #[test]
    fn duplicate_segment_is_not_charged_twice() {
        let mut r = Receiver::new(ReceiverConfig::default());
        r.process_message(segment(0, 0, b"abc"));
        r.process_message(segment(0, 0, b"abc"));
        let t = r.state.transfer(0).unwrap();
        assert_eq!(t.data_bytes, 3);
        assert_eq!(t.held_bytes, 3);
        assert_eq!(r.retained_bytes(), 3 + SEGMENT_OVERHEAD);
    }

    #[test]
    fn segment_over_the_limit_is_counted_but_never_stored() {
        let mut t = InProgressTransfer::new(TransferKind::Core);
        t.insert_segment(0, Bytes::from_static(b"ab"), None, Some(1));
        t.insert_segment(0, Bytes::from_static(b"ab"), None, Some(1));
        assert!(!t.over_segment_limit, "a duplicate is not a new segment");

        t.insert_segment(1, Bytes::from_static(b"cde"), None, Some(1));
        assert!(t.over_segment_limit);
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.data_bytes, 5);
        assert_eq!(t.held_bytes, 2);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut r = receiver(4, usize::MAX);
        r.process_message(segment(7, 0, b"x"));
        r.process_message(segment(8, 0, b"x"));
        r.process_message(Message::TransferCancel { transfer_number: 8 });
        assert_eq!(r.state.window.greatest(), Some(8));

        r.reset();
        assert_eq!(r.state.held.len(), 0);
        assert_eq!(r.retained_bytes(), 0);
        assert!(r.state.closed.is_empty());
        assert_eq!(r.state.window.greatest(), None);
    }

    /// A xorshift generator for the randomised ledger test.  Not
    /// cryptographic, and need not be: it picks messages, not secrets, and
    /// a fixed seed keeps the test deterministic.
    struct XorShift(u64);

    impl XorShift {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    #[test]
    fn retained_always_equals_the_sum_of_held_charges() {
        static DATA: [u8; 64] = [0xAB; 64];
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let mut r = Receiver::new(ReceiverConfig {
            window_size: WindowSize::MIN,
            max_transfer_size: MaxTransferSize::try_from(256).unwrap(),
            max_segments_per_transfer: None,
            max_retained_bytes: MaxRetainedBytes::new(2048),
            fec: true,
        });
        for step in 0..20_000 {
            let transfer_number = rng.below(12) as u32;
            let hints = match rng.below(4) {
                0 => vec![HintItem::BundleLength(rng.below(400))],
                1 => vec![unknown(rng.below(3) as u8, &DATA[..rng.below(9) as usize])],
                _ => vec![],
            };
            let m = TransferSegmentMessage {
                transfer_number,
                segment_index: rng.below(6) as u32,
                hints,
                data: Bytes::from_static(&DATA[..rng.below(65) as usize]),
            };
            let message = match rng.below(9) {
                0..=3 => Message::TransferSegment(m),
                4 | 5 => Message::TransferEnd(m),
                6 => Message::TransferCancel { transfer_number },
                _ => Message::PreAgreedFecSource(PreAgreedFecMessage {
                    transfer_number,
                    fec_instance_id: rng.below(2) as u8,
                    hints: m.hints,
                    payload: m.data,
                }),
            };
            // Half the messages arrive in a PDU of their own, so segments
            // are both kept as views and copied.
            if rng.below(2) == 0 {
                let mut pdu = BytesMut::new();
                encode_message(&message, &mut pdu).unwrap();
                r.receive_pdu(pdu.freeze());
            } else {
                r.process_message(message);
            }
            let held = &r.state.held;
            assert_eq!(
                held.retained,
                held.transfers
                    .values()
                    .map(InProgressTransfer::charge)
                    .sum::<usize>(),
                "after step {step}"
            );
        }
    }
}
