use alloc::{
    boxed::Box,
    collections::{BTreeMap, btree_map::Entry},
    vec::Vec,
};
use core::{
    fmt,
    num::{NonZeroU32, NonZeroUsize},
    str::FromStr,
};

use bytes::{BufMut, Bytes, BytesMut};

use crate::{
    OutOfRange, ParseError,
    codec::{
        BundleExtent, DecodeOptions, Error, decode_pdu_with,
        header::{HEADER_SIZE, MAX_CONTENT_LENGTH},
        hint::{BUNDLE_LENGTH_HINT, HINT_HEADER_SIZE, HintItem, MAX_HINT_TYPE, MAX_HINT_VALUE_LEN},
        message::Message,
    },
    transfer::{Admission, TransferWindow, WindowKey, WindowSize},
};

/// Bookkeeping bytes charged for every stored segment, in addition to the
/// segment's own length, when no [`MaxSegments`] limit is configured.
///
/// It is an estimate of what a stored segment costs beyond its data (the
/// [`Bytes`] handle and its map entry), not an exact heap figure, so that a
/// peer sending many tiny segments cannot hold far more memory than the
/// [`MaxBundleSize`] suggests.  It is rounded down: measured on a 64-bit
/// host, a tiny stored segment costs 70 to 100 bytes after allocator
/// rounding, so a flood can hold up to about half as much again as the
/// charged figure.  The charge counts against a transfer's
/// bookkeeping budget, not against the bundle-size cap itself (see
/// [`MaxBundleSize`]); a flood of empty or one-byte segments is bounded to
/// roughly `max_bundle_size / SEGMENT_OVERHEAD` entries.  The same bound
/// applies to an honest sender, so a link whose segments carry fewer than
/// about `SEGMENT_OVERHEAD` data bytes each needs a [`MaxSegments`] limit
/// to deliver bundles near the cap.
pub const SEGMENT_OVERHEAD: usize = 64;

/// The least bookkeeping budget a transfer has, whatever its
/// [`MaxBundleSize`]: room for 64 stored segments, or the equivalent in
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

/// The most hint charge one transfer can hold: one item of the longest
/// value for every hint type.
const MAX_HINT_CHARGE: usize =
    (MAX_HINT_TYPE as usize + 1) * (HINT_HEADER_SIZE + MAX_HINT_VALUE_LEN);

/// The framing a Transfer Segment or End message spends before its data,
/// hints aside: the message header plus the transfer number and segment
/// index.
const SEGMENT_FRAMING: usize = HEADER_SIZE + 8;

/// The most segment data one message can carry, hints aside.
const MAX_SEGMENT_DATA: usize = MAX_CONTENT_LENGTH - 8;

/// A validated cap on the bundles a receiver will reassemble, in bytes
/// (non-zero).
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
///   segment is charged [`SEGMENT_OVERHEAD`] instead.  Either way the
///   encoded size of every retained hint is charged to a bookkeeping budget
///   of the cap, or [`MIN_OVERHEAD_BUDGET`] if the cap is smaller.
/// - An unsegmented Bundle message is compared against the cap by its
///   length alone, since it is never stored.
///
/// When both conditions fail on the same message, the transfer is reported
/// as `TooLarge`.  Construct via [`MaxBundleSize::new`], [`TryFrom<usize>`],
/// or [`From<NonZeroUsize>`], which enforce the bound at the edge; every
/// consumer of a `MaxBundleSize` can then rely on it.
/// There is no "unlimited" value: a receiver reassembles bundles in
/// memory, so an unbounded cap hands a remote peer a memory-exhaustion
/// lever.  Where no meaningful limit exists, say so explicitly with
/// `usize::MAX`.  The cap is per transfer; see [`Receiver`] for the bound
/// on the receiver as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct MaxBundleSize(NonZeroUsize);

impl MaxBundleSize {
    /// What the value configures, as error messages name it.
    const NAME: &str = "max bundle size";

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

impl Default for MaxBundleSize {
    /// [`MaxBundleSize::DEFAULT`].
    fn default() -> Self {
        Self::DEFAULT
    }
}

forward_integer_fmt!(MaxBundleSize);

impl FromStr for MaxBundleSize {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let v = s.parse::<usize>().map_err(|source| ParseError::Syntax {
            name: Self::NAME,
            source,
        })?;
        Ok(Self::try_from(v)?)
    }
}

impl TryFrom<usize> for MaxBundleSize {
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

impl From<NonZeroUsize> for MaxBundleSize {
    fn from(n: NonZeroUsize) -> Self {
        Self(n)
    }
}

impl From<MaxBundleSize> for NonZeroUsize {
    fn from(m: MaxBundleSize) -> NonZeroUsize {
        m.0
    }
}

impl From<MaxBundleSize> for usize {
    fn from(m: MaxBundleSize) -> usize {
        m.get()
    }
}

/// A validated limit on the distinct segments one transfer may hold
/// (non-zero).
///
/// A transfer that receives a new segment with the limit already reached is
/// rejected as [`RejectReason::TooFragmented`]; repeats of a stored segment
/// never count.  The limit replaces the [`SEGMENT_OVERHEAD`] charge, so a
/// link whose segments carry few data bytes can deliver bundles up to the
/// [`MaxBundleSize`], which that charge would refuse.
///
/// Each stored segment costs roughly 70 to 100 bytes of heap beyond its
/// data (map entry and handle, after allocator rounding), and empty
/// segments cost that too, so a limit of `N` lets one transfer hold about
/// `N × 100` bytes of bookkeeping, up to `window_size` transfers at once,
/// all of them within the [`MaxRetainedBytes`].
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
    /// `max_bundle_size`.
    ///
    /// With `S` the segment data one such PDU carries (the PDU less 12 bytes
    /// of message header, transfer number, and segment index; at least 1, at
    /// most the 20-bit content-length ceiling), the limit is four times
    /// `ceil(max_bundle_size / S)`, never less than 64 and never more than
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
    pub fn for_link_pdu_size(link_pdu_size: usize, max_bundle_size: MaxBundleSize) -> Self {
        let segment_data = link_pdu_size
            .saturating_sub(SEGMENT_FRAMING)
            .clamp(1, MAX_SEGMENT_DATA);
        let reference =
            u32::try_from(max_bundle_size.get().div_ceil(segment_data)).unwrap_or(u32::MAX);
        let limit = reference.saturating_mul(SEGMENT_ALLOWANCE_MULTIPLIER);
        Self::new(limit).map_or(MIN_SEGMENT_ALLOWANCE, |l| l.max(MIN_SEGMENT_ALLOWANCE))
    }
}

forward_integer_fmt!(MaxSegments);

impl FromStr for MaxSegments {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let v = s.parse::<u32>().map_err(|source| ParseError::Syntax {
            name: Self::NAME,
            source,
        })?;
        Ok(Self::try_from(v)?)
    }
}

impl TryFrom<u32> for MaxSegments {
    type Error = OutOfRange;

    fn try_from(v: u32) -> Result<Self, OutOfRange> {
        Self::new(v).ok_or(OutOfRange {
            name: Self::NAME,
            value: u64::from(v),
            min: u64::from(Self::MIN.get()),
            max: None,
        })
    }
}

impl From<NonZeroU32> for MaxSegments {
    fn from(n: NonZeroU32) -> Self {
        Self(n)
    }
}

impl From<MaxSegments> for NonZeroU32 {
    fn from(m: MaxSegments) -> NonZeroU32 {
        m.0
    }
}

impl From<MaxSegments> for u32 {
    fn from(m: MaxSegments) -> u32 {
        m.get()
    }
}

/// A validated limit on the state a [`Receiver`] retains across all of its
/// in-progress transfers, in bytes (non-zero).
///
/// The retained state is what each transfer is charged: its segment bytes,
/// [`SEGMENT_OVERHEAD`] per stored segment, and the encoded size of its
/// retained hints.  Segments are charged here whether or not a
/// [`MaxSegments`] limit replaces the per-transfer segment charge, so
/// empty segments count against the receiver's total too.  A message that
/// would take the total over the limit rejects the transfer it belongs to
/// as [`RejectReason::ReceiverFull`]; transfers already held are kept.  A
/// transfer's charge is released when it is delivered, cancelled, rejected,
/// or expired.
///
/// A receiver never enforces less than [`Self::min_for`] its
/// [`MaxBundleSize`] and [`MaxSegments`], one transfer's full allowance, so
/// a transfer the per-transfer limits admit can always be received on its
/// own; a smaller configured value is raised to it.  Without a configured
/// value, that floor is the limit.
///
/// Like the per-transfer figures, the limit is on charged state, not on
/// heap: a segment kept as a view into its PDU pins up to twice its length
/// (see [`Receiver`]), and allocator rounding adds to each entry.
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

    /// The smallest limit, one byte.  A receiver raises it to
    /// [`Self::min_for`] its per-transfer limits.
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

    /// The least limit a receiver with `max_bundle_size` and
    /// `max_segments` enforces: the most one transfer may be charged,
    /// saturating at `usize::MAX`.
    ///
    /// Without a segment limit that is `max_bundle_size` of segment data
    /// plus the bookkeeping budget its segments and hints share (the cap
    /// again, or [`MIN_OVERHEAD_BUDGET`] if the cap is smaller): twice the
    /// cap for caps of at least 4 KiB, so 2 GiB for the default 1 GiB cap.
    /// With one it is the cap, plus [`SEGMENT_OVERHEAD`] for each segment
    /// the limit allows, plus the most hint bytes a transfer can retain
    /// (about 32 KiB, or the bookkeeping budget if that is smaller).
    pub const fn min_for(
        max_bundle_size: MaxBundleSize,
        max_segments: Option<MaxSegments>,
    ) -> Self {
        let cap = max_bundle_size.0;
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
        Self(cap.saturating_add(bookkeeping))
    }
}

forward_integer_fmt!(MaxRetainedBytes);

impl FromStr for MaxRetainedBytes {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let v = s.parse::<usize>().map_err(|source| ParseError::Syntax {
            name: Self::NAME,
            source,
        })?;
        Ok(Self::try_from(v)?)
    }
}

impl TryFrom<usize> for MaxRetainedBytes {
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

impl From<NonZeroUsize> for MaxRetainedBytes {
    fn from(n: NonZeroUsize) -> Self {
        Self(n)
    }
}

impl From<MaxRetainedBytes> for NonZeroUsize {
    fn from(m: MaxRetainedBytes) -> NonZeroUsize {
        m.0
    }
}

impl From<MaxRetainedBytes> for usize {
    fn from(m: MaxRetainedBytes) -> usize {
        m.get()
    }
}

/// The bookkeeping budget of one transfer under a cap of `max_bundle_size`
/// bytes: the cap itself, or [`MIN_OVERHEAD_BUDGET`] if the cap is smaller.
const fn overhead_budget(max_bundle_size: usize) -> usize {
    if max_bundle_size < MIN_OVERHEAD_BUDGET {
        MIN_OVERHEAD_BUDGET
    } else {
        max_bundle_size
    }
}

/// Configuration for a [`Receiver`].  Every field has a default, so a
/// configuration file may set only what it changes.
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
    pub max_bundle_size: MaxBundleSize,
    /// The most distinct segments one transfer may hold (see
    /// [`MaxSegments`], and [`MaxSegments::for_link_pdu_size`] to derive it
    /// from the link).  Default: `None`, which charges each segment
    /// [`SEGMENT_OVERHEAD`] against the bookkeeping budget instead.
    pub max_segments_per_transfer: Option<MaxSegments>,
    /// The most state the receiver retains across all in-progress
    /// transfers (see [`MaxRetainedBytes`]).  Default: `None`, which
    /// enforces [`MaxRetainedBytes::min_for`] the `max_bundle_size` and
    /// `max_segments_per_transfer`, one transfer's full allowance (2 GiB
    /// for the defaults).
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
}

/// Why the receiver rejected an in-progress transfer, reported by
/// [`ReceiverEvent::TransferRejected`] and then carried by
/// [`DropReason::Rejected`] for the transfer's later messages.
///
/// Deliberately exhaustive, as [`DropReason`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The transfer's bundle exceeds the [`MaxBundleSize`]: the segment
    /// bytes stored, or the sender's Bundle Length hint, are above the cap.
    TooLarge,
    /// The transfer holds far more segments, or more hint data, than the
    /// bundle it could be carrying justifies: it exceeded its
    /// [`MaxSegments`] limit, or its bookkeeping ([`SEGMENT_OVERHEAD`] per
    /// stored segment when no limit is configured, plus retained hint
    /// bytes) exceeds the [`MaxBundleSize`] on its own.
    TooFragmented,
    /// The transfer mixed core Transfer Segment or End messages with FEC
    /// messages, which the FEC extension forbids (Section 3.2 of
    /// draft-ietf-dtn-btpu-fec); the receiver treats the transfer as
    /// cancelled.
    FecCoreMixing,
    /// An FEC transfer's FEC configuration changed mid-transfer: a different
    /// pre-agreed FEC Instance ID, a different explicit FEC Encoding ID, or a
    /// switch between the pre-agreed and explicit forms (Sections 3 and 3.1
    /// of draft-ietf-dtn-btpu-fec).  The receiver treats the transfer as
    /// cancelled.  Scheme-specific information is not compared, since no
    /// FEC scheme is implemented.
    FecConfigurationChanged,
    /// Every segment of the transfer arrived but none held data.  Zero bytes
    /// cannot be a valid bundle: an empty Bundle Message is rejected under
    /// Section 8.1, and the same policy applies to a reassembled transfer.
    EmptyBundle,
    /// The message would have taken the state the receiver retains across
    /// all transfers over its [`MaxRetainedBytes`].  The transfer the
    /// message belongs to is the one refused; transfers already held are
    /// kept.
    ReceiverFull,
}

impl From<RejectReason> for DropReason {
    fn from(reason: RejectReason) -> Self {
        Self::Rejected(reason)
    }
}

/// Events emitted by the receiver for the calling CLA to act on.
///
/// Deliberately exhaustive: values are produced by this crate, never decoded
/// from the wire, and a consumer that acts per-variant should get a compile
/// error when one is added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverEvent {
    /// A complete bundle has been reassembled, or received whole as a Bundle
    /// message or an encapsulated bundle.
    BundleReceived {
        /// The bundle bytes.
        data: Bytes,
        /// The transfer's hint items (Section 7.2), including hint types
        /// this implementation does not recognise, so extension metadata (a
        /// correlator, say) reaches the caller without an API change.  One
        /// item per hint type, in ascending hint-type order: hints are
        /// transfer-scoped and repeatable, and the value from the most
        /// recently received message carrying a type supersedes any earlier
        /// one, whether across the messages of a transfer or within a
        /// single Bundle message.  A Bundle Length hint on a Bundle message
        /// is omitted (Section 9.1: receivers SHOULD ignore it there).
        ///
        /// A transfer's Bundle Length hint is passed on as the sender wrote
        /// it.  It is advisory: the receiver uses it only to reject an
        /// oversized transfer early, and it can disagree with `data`,
        /// whose length is the authoritative one.
        hints: Vec<HintItem>,
    },

    /// A transfer was cancelled by the sender.
    ///
    /// Also reported for a Cancel of an in-window number the receiver has
    /// seen nothing of: Section 5 counts every number in the window as in
    /// progress, and Section 8.4 has segments arriving after the Cancel
    /// discarded, so the number is remembered as cancelled either way.
    TransferCancelled { transfer_number: u32 },

    /// A transfer was evicted from the window (incomplete).  One window
    /// advance can evict several; they are reported oldest first.
    TransferExpired { transfer_number: u32 },

    /// A message was dropped without being applied to any transfer.
    /// Informational: the caller decides whether this matters (statistics,
    /// logging, or nothing at all).
    MessageDropped {
        transfer_number: u32,
        reason: DropReason,
    },

    /// An in-progress transfer was rejected and its state discarded; later
    /// messages for it are dropped as [`DropReason::Rejected`] with the
    /// same reason.  Distinct from
    /// [`Self::TransferCancelled`], which reports a sender's Transfer Cancel,
    /// although a transfer rejected for a protocol violation is one the
    /// drafts call cancelled.
    TransferRejected {
        transfer_number: u32,
        reason: RejectReason,
    },

    /// An unsegmented Bundle message was rejected by local policy: its
    /// content exceeds the configured [`MaxBundleSize`], or it is empty
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
    MalformedMessage { error: Error },

    /// The PDU could not be walked further (no message boundary could be
    /// determined, or an encapsulated bundle of unknown extent was reached,
    /// Section 7.3) and the remainder was discarded.  Always the final event
    /// of its PDU.
    ///
    /// Positions in `error` count from the start of the PDU passed to
    /// [`Receiver::receive_pdu`] or [`Receiver::receive_pdu_into`]; a caller
    /// that wants to inspect the discarded bytes keeps a clone of that
    /// `Bytes` (a reference-count increment, not a copy).
    MalformedPdu { error: Error },
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
}

/// Fold hint items into a per-type map, a later item superseding an earlier
/// one of the same type.  The single definition of the "one item per hint
/// type, latest wins" rule behind [`ReceiverEvent::BundleReceived`], shared
/// by the transfer path (accumulating across messages) and the Bundle
/// message path (one message).
fn fold_hints(into: &mut BTreeMap<u8, HintItem>, hints: impl IntoIterator<Item = HintItem>) {
    for h in hints {
        into.insert(h.hint_type(), h);
    }
}

/// Reduce one message's hint list to the delivered form: one item per hint
/// type, latest wins, in ascending hint-type order.
fn dedup_hints(hints: impl IntoIterator<Item = HintItem>) -> Vec<HintItem> {
    let mut map = BTreeMap::new();
    fold_hints(&mut map, hints);
    map.into_values().collect()
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
            value: Bytes::copy_from_slice(&value),
        },
        other => other,
    }
}

/// Detach a segment from its PDU when keeping the view would pin far more
/// memory than the segment is worth.
///
/// A stored segment that is a view into its PDU keeps the whole PDU
/// allocation alive.  Segments of at least half the PDU are kept as views
/// (they pin at most twice their length); shorter ones are copied.  A
/// message not decoded from a PDU (`pdu_len` is `None`) is stored as given.
fn retain_segment(data: Bytes, pdu_len: Option<usize>) -> Bytes {
    match pdu_len {
        Some(pdu_len) if data.len() < pdu_len / 2 => Bytes::copy_from_slice(&data),
        _ => data,
    }
}

struct InProgressTransfer {
    kind: TransferKind,
    segments: BTreeMap<u32, Bytes>,
    final_segment_index: Option<u32>,
    /// The transfer's hint items, keyed by hint type: hints are
    /// transfer-scoped and repeatable (Section 7.2), so the value from the
    /// most recently received message supersedes an earlier one.
    /// Structurally bounded, at most 2^7 types of at most 255 value bytes
    /// each, and charged to `overhead` besides.
    hints: BTreeMap<u8, HintItem>,
    /// Segment bytes received so far, including a segment refused for
    /// exceeding the segment limit: the bundle's length as far as it is
    /// known, compared against the [`MaxBundleSize`].
    data_bytes: usize,
    /// Bookkeeping charged so far: [`SEGMENT_OVERHEAD`] per stored segment
    /// when no segment limit applies, plus the encoded size of every
    /// retained hint, compared against the [`MaxBundleSize`] separately
    /// from the bundle's length.
    overhead: usize,
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
            hints: BTreeMap::new(),
            data_bytes: 0,
            overhead: 0,
            over_segment_limit: false,
        }
    }

    /// Insert a segment unless it is a duplicate (repetition support).
    ///
    /// With a `segment_limit`, a new segment that would exceed it is not
    /// stored but its bytes are still counted, so [`Self::exceeds`] can
    /// prefer [`RejectReason::TooLarge`] when the bundle is over both limits.
    /// Without one, each stored segment is charged [`SEGMENT_OVERHEAD`].
    fn insert_segment(&mut self, index: u32, data: Bytes, segment_limit: Option<u64>) {
        // usize is at most 64 bits on every supported target.
        let stored = self.segments.len() as u64;
        let Entry::Vacant(e) = self.segments.entry(index) else {
            return;
        };
        self.data_bytes = self.data_bytes.saturating_add(data.len());
        match segment_limit {
            Some(limit) if stored >= limit => self.over_segment_limit = true,
            Some(_) => {
                e.insert(data);
            }
            None => {
                self.overhead = self.overhead.saturating_add(SEGMENT_OVERHEAD);
                e.insert(data);
            }
        }
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

    /// Which [`MaxBundleSize`] rule the transfer provably breaks, if any:
    /// [`RejectReason::TooLarge`] when the bytes received or the sender's
    /// Bundle Length hint exceed `max`, [`RejectReason::TooFragmented`] when a
    /// segment was refused by the segment limit or the bookkeeping alone
    /// exceeds its budget.
    fn exceeds(&self, max: usize) -> Option<RejectReason> {
        if self.data_bytes > max || self.bundle_length_hint().is_some_and(|h| h > max as u64) {
            Some(RejectReason::TooLarge)
        } else if self.over_segment_limit || self.overhead > overhead_budget(max) {
            Some(RejectReason::TooFragmented)
        } else {
            None
        }
    }

    /// What the transfer counts against the receiver's [`MaxRetainedBytes`]:
    /// its segment bytes, [`SEGMENT_OVERHEAD`] per stored segment whatever
    /// the segment limit, and its hints.  Sums the hints (at most 128) on
    /// each call, like [`Self::hint_charge`].
    fn charge(&self) -> usize {
        self.data_bytes
            .saturating_add(self.segments.len().saturating_mul(SEGMENT_OVERHEAD))
            .saturating_add(self.hint_charge())
    }

    /// The sender's Bundle Length hint, if one has been received.
    fn bundle_length_hint(&self) -> Option<u64> {
        match self.hints.get(&BUNDLE_LENGTH_HINT) {
            Some(HintItem::BundleLength(len)) => Some(*len),
            _ => None,
        }
    }

    /// Record hints from a message, keeping the latest value per hint type,
    /// and re-charge the retained set: a superseded value is released and
    /// its replacement charged, so the charge always matches what is held.
    fn apply_hints(&mut self, hints: Vec<HintItem>) {
        if hints.is_empty() {
            return;
        }
        let before = self.hint_charge();
        fold_hints(&mut self.hints, hints.into_iter().map(own_hint));
        self.overhead = self
            .overhead
            .saturating_sub(before)
            .saturating_add(self.hint_charge());
    }

    /// The bookkeeping charge of the retained hints: each item's encoded
    /// size, which is what its value copy and map entry cost in kind.  At
    /// most 128 items, so summing on demand is cheap.
    fn hint_charge(&self) -> usize {
        self.hints
            .values()
            .map(|h| match h {
                HintItem::BundleLength(_) => HINT_HEADER_SIZE + 8,
                HintItem::Unknown { value, .. } => HINT_HEADER_SIZE + value.len(),
            })
            .sum()
    }

    /// Check whether all segments 0..=N have been received.
    fn is_complete(&self) -> bool {
        let Some(n) = self.final_segment_index else {
            return false;
        };
        // Count in u64: `n` is wire-supplied, so `n + 1` overflows u32 when a
        // hostile End claims a final index of u32::MAX.
        self.segments.len() as u64 == u64::from(n) + 1
            && self.segments.last_key_value().map(|(k, _)| *k) == Some(n)
            && self.segments.first_key_value().map(|(k, _)| *k) == Some(0)
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
    fn reassemble(&mut self) -> Bytes {
        if self.segments.len() == 1 {
            return self.segments.pop_first().expect("length checked").1;
        }
        let total = self
            .segments
            .values()
            .fold(0usize, |acc, s| acc.saturating_add(s.len()));
        let mut buf = BytesMut::with_capacity(total);
        for data in self.segments.values() {
            buf.put_slice(data);
        }
        buf.freeze()
    }
}

/// Manages inbound PDU processing, transfer window, and segment reassembly.
///
/// # Memory
///
/// Each in-progress transfer is bounded by the [`MaxBundleSize`]: its
/// stored segment bytes may not exceed the cap, its retained hint bytes
/// may not exceed the cap either (or [`MIN_OVERHEAD_BUDGET`], for a small
/// cap), and its segment count is limited.  Without a [`MaxSegments`]
/// limit the count is limited by charging [`SEGMENT_OVERHEAD`] per segment
/// to the same budget as the hints, so a transfer holds at most twice the
/// cap of charged state.  With one, the count is limited directly and the
/// per-segment cost is on top: roughly 70 to 100 bytes per segment, empty
/// ones included (see [`MaxSegments`]).  A segment
/// shorter than half its PDU is copied out rather than pinning the PDU, and
/// one at least that long stays a view that pins at most twice its length,
/// provided the caller hands over each PDU in a buffer of its own size (a
/// `Bytes` split from a larger receive buffer pins all of it).  Up to
/// `window_size` transfers can be in progress at once, and together they
/// are charged at most the [`MaxRetainedBytes`], which by default is one
/// transfer's full allowance.  These figures are estimates of retained
/// state, not a bound on all receiver memory: receive buffers, returned
/// events, and allocator overhead are separate.  The numbers of delivered, cancelled,
/// and rejected transfers are remembered until the window passes them,
/// which the window size also bounds.
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
    max_bundle_size: MaxBundleSize,
    /// [`ReceiverConfig::max_segments_per_transfer`], widened once for the
    /// comparison against a segment count.
    segment_limit: Option<u64>,
    /// The limit in force: [`ReceiverConfig::max_retained_bytes`] raised to
    /// [`MaxRetainedBytes::min_for`] the per-transfer limits.
    max_retained_bytes: MaxRetainedBytes,
    /// The sum of every in-progress transfer's charge, kept equal to it by
    /// [`Self::process_transfer_message`] (which charges) and
    /// [`Self::remove_transfer`] and window expiry (which release).
    retained: usize,
    fec: bool,
    window: TransferWindow,
    /// Keyed in window order, oldest first, so a window advance expires a
    /// leading run of entries and costs only what it expires.
    transfers: BTreeMap<WindowKey, InProgressTransfer>,
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
            .field("max_bundle_size", &state.max_bundle_size)
            .field("segment_limit", &state.segment_limit)
            .field("max_retained_bytes", &state.max_retained_bytes)
            .field("retained", &state.retained)
            .field("fec", &state.fec)
            .field("bundle_extent", &self.bundle_extent.is_some())
            .field("window", &state.window)
            .field("transfers", &state.transfers.len())
            .field("closed", &state.closed)
            .finish()
    }
}

impl Receiver {
    /// Create a new receiver.
    ///
    /// Transfers that provably exceed the configured [`MaxBundleSize`] or
    /// segment allowance, that break the FEC extension's rules, or that
    /// complete with no data are rejected with
    /// [`ReceiverEvent::TransferRejected`], and oversized or empty Bundle
    /// messages with [`ReceiverEvent::BundleRejected`].  A
    /// [`ReceiverConfig::max_retained_bytes`] below
    /// [`MaxRetainedBytes::min_for`] the per-transfer limits is raised to it.
    pub fn new(config: ReceiverConfig) -> Self {
        let floor =
            MaxRetainedBytes::min_for(config.max_bundle_size, config.max_segments_per_transfer);
        Self {
            state: Reassembly {
                max_bundle_size: config.max_bundle_size,
                segment_limit: config.max_segments_per_transfer.map(|m| u64::from(m.get())),
                max_retained_bytes: config.max_retained_bytes.map_or(floor, |m| m.max(floor)),
                retained: 0,
                fec: config.fec,
                window: TransferWindow::new(config.window_size),
                transfers: BTreeMap::new(),
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
        state.transfers.clear();
        state.closed.clear();
        state.retained = 0;
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
                if data.is_empty() || data.len() > self.max_bundle_size.get() {
                    events.push(ReceiverEvent::BundleRejected { len: data.len() });
                    return;
                }
                // Section 9.1: a Bundle Length hint is only meaningful on
                // Transfer Segment and End messages and SHOULD be ignored
                // elsewhere.
                let hints = dedup_hints(
                    hints
                        .into_iter()
                        .filter(|h| h.hint_type() != BUNDLE_LENGTH_HINT),
                );
                events.push(ReceiverEvent::BundleReceived { data, hints });
            }

            Message::TransferSegment(m) => self.process_transfer_message(
                m.transfer_number,
                TransferKind::Core,
                m.hints,
                Some(CoreSegment {
                    index: m.segment_index,
                    data: m.data,
                    end: false,
                    pdu_len,
                }),
                events,
            ),

            Message::TransferEnd(m) => self.process_transfer_message(
                m.transfer_number,
                TransferKind::Core,
                m.hints,
                Some(CoreSegment {
                    index: m.segment_index,
                    data: m.data,
                    end: true,
                    pdu_len,
                }),
                events,
            ),

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
    /// or the receiver over: the transfer's [`MaxBundleSize`] rules (see
    /// [`InProgressTransfer::exceeds`]), then the receiver's
    /// [`MaxRetainedBytes`] as [`RejectReason::ReceiverFull`].
    ///
    /// Checked after each state change, so a transfer is rejected as soon
    /// as it breaks a limit, or earlier still if the sender's Bundle Length
    /// hint already promises an oversized bundle.  A transfer within its
    /// own limits is blamed for the receiver's total because the total was
    /// within its limit before the change, so rejecting the transfer that
    /// grew brings it back.
    fn over_limit(&self, key: WindowKey) -> Option<RejectReason> {
        self.transfers
            .get(&key)
            .and_then(|t| t.exceeds(self.max_bundle_size.get()))
            .or_else(|| {
                (self.retained > self.max_retained_bytes.get())
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

    /// Discard a transfer's state and remember it as closed with `reason`,
    /// so later messages for it are dropped rather than re-opening it (the
    /// Section 4.2 treatment of a cancelled transfer).
    fn reject_transfer(
        &mut self,
        key: WindowKey,
        reason: RejectReason,
        events: &mut Vec<ReceiverEvent>,
    ) {
        self.remove_transfer(key);
        self.closed.insert(key, reason.into());
        events.push(ReceiverEvent::TransferRejected {
            transfer_number: key.transfer_number(),
            reason,
        });
    }

    /// Shared pipeline for every message that opens or extends a transfer
    /// (Segment, End, and the four FEC messages): admission, transfer-kind
    /// check, sequence-conflict check, state application, oversize gate,
    /// completion check.  One path for all of them keeps the gate ordering
    /// (and any future fix to it) in lockstep.
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

        let transfer = self
            .transfers
            .entry(key)
            .or_insert_with(|| InProgressTransfer::new(kind));

        if let Some(reason) = transfer.kind_mismatch(kind) {
            return self.reject_transfer(key, reason, events);
        }

        // A conflicting message is dropped with no state touched, hints
        // included.
        if segment.as_ref().is_some_and(|s| s.conflicts(transfer)) {
            return Self::drop_message(transfer_number, DropReason::SegmentIndexConflict, events);
        }

        let before = transfer.charge();
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
            // An empty segment (Section 8.2 SHOULD NOT), or an empty End
            // (Section 8.3 SHOULD carry data), is still a segment: Section 4
            // completes a transfer once indices 0..=N are present, so it is
            // stored, and the segment limit (or SEGMENT_OVERHEAD) keeps a
            // flood of them bounded.  A streaming sender that only learns
            // the end of its input after emitting a full segment has no
            // other way to finish.
            transfer.insert_segment(index, retain_segment(data, pdu_len), self.segment_limit);
        }
        self.retained = self
            .retained
            .saturating_sub(before)
            .saturating_add(transfer.charge());

        if let Some(reason) = self.over_limit(key) {
            return self.reject_transfer(key, reason, events);
        }

        // A late segment may fill the final gap of a transfer whose End was
        // already received; check completeness after every apply, not just
        // after an End.  An FEC transfer never has a final index, so for it
        // this is a no-op.
        self.complete_if_ready(key, events);
    }

    /// If the transfer's segments are all present (and its final index is
    /// known), reassemble it, remove it from the window, and push a
    /// `BundleReceived` event.  A no-op otherwise.  Called after every segment
    /// or End insert so out-of-order completion is detected regardless of which
    /// message arrives last.
    fn complete_if_ready(&mut self, key: WindowKey, events: &mut Vec<ReceiverEvent>) {
        let complete = self
            .transfers
            .get(&key)
            .is_some_and(InProgressTransfer::is_complete);
        if !complete {
            return;
        }

        // No max_bundle_size check needed here: over_limit enforces it on
        // every insert, so a transfer that reaches completion is within limit.
        // Zero bytes cannot be a valid bundle; the policy that rejects an
        // empty Bundle Message (Section 8.1) applies to a reassembled one.
        if self.transfers.get(&key).is_some_and(|t| t.data_bytes == 0) {
            self.reject_transfer(key, RejectReason::EmptyBundle, events);
            return;
        }
        let Some(mut transfer) = self.remove_transfer(key) else {
            return;
        };
        // A sender may repeat any message (Section 6), so the transfer's
        // repeats can still arrive; remembering it keeps them from opening
        // a second copy that would deliver the bundle twice or sit in the
        // window as a phantom.
        self.closed.insert(key, DropReason::Delivered);
        events.push(ReceiverEvent::BundleReceived {
            data: transfer.reassemble(),
            hints: transfer.hints.into_values().collect(),
        });
    }

    fn process_transfer_cancel(&mut self, transfer_number: u32, events: &mut Vec<ReceiverEvent>) {
        // Section 8.4: a Cancel that does not match an in-progress transfer
        // MUST be ignored, and Section 5 defines "in progress" by the
        // window range alone.  So a Cancel never advances the window (no
        // admit here: "ignored" includes side effects), and one
        // for an in-window number is applied whether or not any of the
        // transfer's segments have arrived yet.  Remembering it is what
        // makes Section 8.4's "prior or later received Segments MUST be
        // discarded" hold when the segments are reordered behind the
        // Cancel or repeated after it.
        // Nothing outside the window is held, so a number outside it can
        // only be unknown.
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
        self.remove_transfer(key);
        self.closed.insert(key, DropReason::Cancelled);
        events.push(ReceiverEvent::TransferCancelled { transfer_number });
    }

    /// Drop every transfer, live or closed, that the window has moved past,
    /// reporting the live ones as [`ReceiverEvent::TransferExpired`] oldest
    /// first.  Both maps are keyed in window order, so the expired entries
    /// are a leading run and the cost is O(log n) per entry expired, not a
    /// walk of the window.
    fn expire_old_transfers(&mut self, events: &mut Vec<ReceiverEvent>) {
        debug_assert!(
            self.transfers
                .last_key_value()
                .is_none_or(|(&k, _)| self.window.is_behind_greatest(k))
                && self
                    .closed
                    .last_key_value()
                    .is_none_or(|(&k, _)| self.window.is_behind_greatest(k)),
            "a held transfer is ahead of the window"
        );
        while let Some(entry) = self.transfers.first_entry()
            && self.window.is_expired(*entry.key())
        {
            let (key, transfer) = entry.remove_entry();
            self.retained = self.retained.saturating_sub(transfer.charge());
            events.push(ReceiverEvent::TransferExpired {
                transfer_number: key.transfer_number(),
            });
        }

        // Prune the closed map the same way; this is what keeps it bounded
        // by the window size.  No events: these were already reported as
        // BundleReceived / TransferCancelled / TransferRejected when they
        // closed.
        while let Some(entry) = self.closed.first_entry()
            && self.window.is_expired(*entry.key())
        {
            entry.remove();
        }
    }

    /// Remove an in-progress transfer, releasing its charge from the
    /// receiver's total.
    fn remove_transfer(&mut self, key: WindowKey) -> Option<InProgressTransfer> {
        let transfer = self.transfers.remove(&key)?;
        self.retained = self.retained.saturating_sub(transfer.charge());
        Some(transfer)
    }

    /// The in-progress transfer numbered `transfer_number`, looked up
    /// through the window as production code does.
    #[cfg(test)]
    fn transfer(&self, transfer_number: u32) -> Option<&InProgressTransfer> {
        self.transfers.get(&self.window.key(transfer_number)?)
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
    use crate::codec::message::TransferSegmentMessage;

    fn receiver(window_size: u16, max_bundle_size: usize) -> Receiver {
        Receiver::new(ReceiverConfig {
            window_size: WindowSize::try_from(window_size).unwrap(),
            max_bundle_size: MaxBundleSize::try_from(max_bundle_size).unwrap(),
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
    fn completed_transfer_is_closed_not_forgotten() {
        let mut r = Receiver::new(ReceiverConfig::default());
        r.process_message(end(0, 1, b"ld"));
        assert!(r.state.transfer(0).is_some());
        let events = r.process_message(segment(0, 0, b"wor"));
        assert_eq!(
            events,
            vec![ReceiverEvent::BundleReceived {
                data: Bytes::from_static(b"world"),
                hints: vec![],
            }]
        );
        assert!(r.state.transfer(0).is_none());
        assert_eq!(r.state.closed_reason(0), Some(&DropReason::Delivered));
    }

    #[test]
    fn final_segment_index_of_u32_max_does_not_overflow() {
        let mut r = Receiver::new(ReceiverConfig::default());
        // A hostile End claiming u32::MAX as the final index must not
        // overflow the completeness check; the transfer just stays
        // incomplete (2^32 segments can never all be present).
        let events = r.process_message(end(0, u32::MAX, b"end"));
        assert_eq!(events, vec![]);
        assert!(r.state.transfer(0).is_some());
    }

    #[test]
    fn cancel_removes_transfer_and_remembers_it() {
        let mut r = Receiver::new(ReceiverConfig::default());
        r.process_message(segment(5, 0, b"data"));
        let events = r.process_message(Message::TransferCancel { transfer_number: 5 });
        assert_eq!(
            events,
            vec![ReceiverEvent::TransferCancelled { transfer_number: 5 }]
        );
        assert!(r.state.transfer(5).is_none());
        assert_eq!(r.state.closed_reason(5), Some(&DropReason::Cancelled));

        // A repeated segment (Section 6 repetition) after the Cancel MUST
        // NOT re-create the transfer (Section 4.2).
        let events = r.process_message(segment(5, 0, b"data"));
        assert_eq!(
            events,
            vec![ReceiverEvent::MessageDropped {
                transfer_number: 5,
                reason: DropReason::Cancelled,
            }]
        );
        assert!(r.state.transfer(5).is_none());
    }

    #[test]
    fn closed_map_pruned_by_window_advance() {
        let mut r = receiver(4, usize::MAX);
        r.process_message(segment(0, 0, b"x"));
        r.process_message(Message::TransferCancel { transfer_number: 0 });
        assert!(r.state.closed_reason(0).is_some());

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

    #[test]
    fn window_expiry_evicts_exactly_the_oldest() {
        let mut r = receiver(4, usize::MAX);
        for t in 0..4u32 {
            r.process_message(segment(t, 0, b"x"));
        }
        assert_eq!(r.state.transfers.len(), 4);

        // New transfer 4 expires transfer 0 and nothing else.
        let events = r.process_message(segment(4, 0, b"y"));
        assert_eq!(
            events,
            vec![ReceiverEvent::TransferExpired { transfer_number: 0 }]
        );
        assert_eq!(r.state.transfers.len(), 4);
        assert!(r.state.transfer(0).is_none());
    }

    #[test]
    fn oversized_transfer_rejected_during_accumulation() {
        let mut r = receiver(16, 5);

        // 3 bytes: under the limit, transfer stays alive.
        r.process_message(segment(0, 0, b"abc"));
        assert!(r.state.transfer(0).is_some());

        // 6 accumulated bytes: rejected immediately, no waiting for the End.
        let events = r.process_message(segment(0, 1, b"def"));
        assert_eq!(
            events,
            vec![ReceiverEvent::TransferRejected {
                transfer_number: 0,
                reason: RejectReason::TooLarge,
            }]
        );
        assert!(r.state.transfer(0).is_none());
        assert_eq!(
            r.state.closed_reason(0),
            Some(&DropReason::Rejected(RejectReason::TooLarge))
        );

        // A further segment must not re-create the rejected transfer.
        let events = r.process_message(segment(0, 2, b"x"));
        assert!(r.state.transfer(0).is_none());
        assert_eq!(
            events,
            vec![ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::Rejected(RejectReason::TooLarge),
            }]
        );
    }

    #[test]
    fn segments_and_hints_are_charged_to_overhead_not_data() {
        let mut r = Receiver::new(ReceiverConfig::default());
        r.process_message(segment(0, 0, b"a"));
        r.process_message(segment(0, 1, b""));
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 2,
            hints: vec![
                HintItem::BundleLength(3),
                HintItem::Unknown {
                    hint_type: 0x41,
                    value: Bytes::from_static(b"corr"),
                },
            ],
            data: Bytes::from_static(b"b"),
        }));
        let t = &r.state.transfer(0).unwrap();
        assert_eq!(t.segments.len(), 3);
        assert_eq!(t.data_bytes, 2);
        assert_eq!(
            t.overhead,
            3 * SEGMENT_OVERHEAD + (HINT_HEADER_SIZE + 8) + (HINT_HEADER_SIZE + 4)
        );

        // Superseding a hint releases the old value's charge.
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 3,
            hints: vec![HintItem::Unknown {
                hint_type: 0x41,
                value: Bytes::from_static(b"c"),
            }],
            data: Bytes::from_static(b"c"),
        }));
        let t = &r.state.transfer(0).unwrap();
        assert_eq!(
            t.overhead,
            4 * SEGMENT_OVERHEAD + (HINT_HEADER_SIZE + 8) + (HINT_HEADER_SIZE + 1)
        );
    }

    #[test]
    fn duplicate_segment_is_not_charged_twice() {
        let mut r = Receiver::new(ReceiverConfig::default());
        r.process_message(segment(0, 0, b"abc"));
        r.process_message(segment(0, 0, b"abc"));
        assert_eq!(r.state.transfer(0).unwrap().data_bytes, 3);
        assert_eq!(r.state.transfer(0).unwrap().overhead, SEGMENT_OVERHEAD);
    }

    #[test]
    fn segment_over_the_limit_is_counted_but_never_stored() {
        let mut t = InProgressTransfer::new(TransferKind::Core);
        t.insert_segment(0, Bytes::from_static(b"ab"), Some(1));
        t.insert_segment(0, Bytes::from_static(b"ab"), Some(1));
        assert!(!t.over_segment_limit, "a duplicate is not a new segment");

        t.insert_segment(1, Bytes::from_static(b"cde"), Some(1));
        assert!(t.over_segment_limit);
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.data_bytes, 5);
        // The limit replaces the per-segment charge.
        assert_eq!(t.overhead, 0);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut r = receiver(4, usize::MAX);
        r.process_message(segment(7, 0, b"x"));
        r.process_message(segment(8, 0, b"x"));
        r.process_message(Message::TransferCancel { transfer_number: 8 });
        assert_eq!(r.state.window.greatest(), Some(8));

        r.reset();
        assert!(r.state.transfers.is_empty());
        assert!(r.state.closed.is_empty());
        assert_eq!(r.state.window.greatest(), None);
    }
}
