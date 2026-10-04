//! Receiver limits and configuration.

use core::num::{NonZeroU32, NonZeroUsize};

#[cfg(doc)]
use bytes::Bytes;

#[cfg(doc)]
use super::{Receiver, ReceiverEvent, RejectReason};
#[cfg(doc)]
use crate::codec::DecodeOptions;
use crate::{
    codec::{
        header::MAX_CONTENT_LENGTH,
        hint::{HINT_TYPE_COUNT, HintType, HintValue, MAX_HINT_VALUE_LEN},
        message::{SEGMENT_FIELDS_SIZE, SEGMENT_FRAMING},
    },
    transfer::WindowSize,
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
pub const MIN_SEGMENT_ALLOWANCE: MaxSegments =
    MaxSegments(NonZeroU32::new((MIN_OVERHEAD_BUDGET / SEGMENT_OVERHEAD) as u32).unwrap());

/// How many times the reference segment count
/// [`MaxSegments::for_link_pdu_size`] allows.
///
/// Headroom for segments that do not fill their PDU: the first segment's
/// hints, packing remainders, and a sender interleaving transfers within a
/// PDU (Section 4.1), where two-way interleaving alone halves every
/// segment.
pub const SEGMENT_ALLOWANCE_MULTIPLIER: u32 = 4;

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
pub const MAX_HINT_CHARGE: usize = HINT_TYPE_COUNT * (HINT_OVERHEAD + MAX_HINT_VALUE_LEN);

/// The most segment data one message can carry, hints aside.
pub const MAX_SEGMENT_DATA: usize = MAX_CONTENT_LENGTH - SEGMENT_FIELDS_SIZE;

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
pub const fn overhead_budget(max_transfer_size: usize) -> usize {
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
    /// How a segmented transfer's bundle is handed over.  Default:
    /// [`Delivery::Whole`].
    pub delivery: Delivery,
}

/// How a [`Receiver`] hands over the bundle of a segmented transfer.
///
/// Unsegmented bundles (Bundle messages and bare or encapsulated frames)
/// arrive whole and are reported as [`ReceiverEvent::Received`] either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "kebab-case")
)]
pub enum Delivery {
    /// Hold a transfer until every segment has arrived, then report the
    /// reassembled bundle as one [`ReceiverEvent::Received`].
    #[default]
    Whole,
    /// Release a transfer's bytes as soon as they are contiguous from its
    /// start: [`ReceiverEvent::TransferStarted`], then
    /// [`ReceiverEvent::TransferData`] for each released segment, then
    /// [`ReceiverEvent::TransferFinished`].  A released segment is no
    /// longer held, so in-order arrival costs the receiver little beyond
    /// the segment being released, and the bundle is never copied into
    /// one buffer.
    ///
    /// The receiver cannot push back on the link.  Released bytes that the
    /// consumer has not yet read are outside the receiver's limits, so a
    /// CLA that buffers them bounds the buffer and calls
    /// [`Receiver::refuse`] when it is full, rather than blocking the loop
    /// that feeds the receiver.
    Streamed,
}
