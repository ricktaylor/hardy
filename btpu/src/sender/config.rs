//! Sender configuration.

use core::num::NonZeroUsize;

#[cfg(doc)]
use bytes::Bytes;

#[cfg(doc)]
use super::{Error, Sender};
#[cfg(doc)]
use crate::codec::message::frame_kind;
use crate::{
    codec::header::{HEADER_SIZE, MAX_CONTENT_LENGTH},
    transfer::WindowSize,
};

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

/// The backpressure threshold on the bytes a [`Sender`] holds queued and
/// not yet packed into a PDU, counting whole bundles from
/// [`Sender::enqueue`] and chunks from [`Sender::push`] alike.
///
/// The watermark drives backpressure, not errors: the `tower`
/// `Service::poll_ready` returns `Pending` while the queue is at or above
/// it, and direct callers pace themselves with
/// [`Sender::is_send_queue_high`] and by draining [`Sender::next_pdu`].
/// `enqueue` and `push` never refuse on it, and take the whole bundle or
/// chunk they are given, so one call below the watermark can take the
/// queue past it by any amount; a bundle larger than the watermark is
/// still taken.
///
/// It is not a memory limit.  The count is of bundle bytes, not of the
/// allocations behind them: a [`Bytes`] keeps its whole backing buffer
/// alive, so a queued slice of a larger buffer holds more memory than it
/// counts, and two queued clones of one buffer count twice.
///
/// Construct via [`SendQueueHighWatermark::new`], [`TryFrom<usize>`], or
/// [`From<NonZeroUsize>`]; a zero watermark would park `poll_ready`
/// forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct SendQueueHighWatermark(NonZeroUsize);

impl SendQueueHighWatermark {
    /// What the value configures, as error messages name it.
    const NAME: &str = "send queue high watermark";

    /// The smallest watermark, one byte.
    pub const MIN: Self = Self(NonZeroUsize::MIN);

    /// The largest watermark, `usize::MAX` bytes.
    pub const MAX: Self = Self(NonZeroUsize::MAX);

    /// 1 MiB.
    pub const DEFAULT: Self = Self(NonZeroUsize::new(1 << 20).unwrap());

    /// Returns the watermark for `bytes`, or `None` if it is zero.
    pub const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Returns the watermark as a plain integer.
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for SendQueueHighWatermark {
    fn default() -> Self {
        Self::DEFAULT
    }
}

config_newtype!(SendQueueHighWatermark: usize, NonZeroUsize);

/// The framing discipline of the link a [`Sender`] feeds.
///
/// It decides how far [`Sender::next_pdu`] pads each PDU and whether a
/// fitting bundle may travel without a BTP-U header.  The combination
/// "fixed-size frames with bare bundles" is unrepresentable: a bare frame is
/// the bundle's own bytes, so it cannot be padded.
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
    /// Variable-length PDUs (for example UDP datagrams or Ethernet
    /// frames): the [`PduSize`] is a ceiling, a PDU is padded only up to
    /// `min_pdu_len`, and `bundle_framing` chooses how a fitting bundle is
    /// put on the wire.
    Variable {
        /// How a bundle that fits in one PDU is framed.  Default: a Bundle
        /// Message, so a configuration file may say `variable: {}`.
        #[cfg_attr(feature = "serde", serde(default))]
        bundle_framing: BundleFraming,
        /// The length a shorter PDU is padded up to, with Definite Padding
        /// (and Indefinite Padding for a gap of under four bytes).  Default:
        /// 0, no padding.
        ///
        /// Ethernet wants 46, its minimum frame payload, so that the
        /// receiver never sees the MAC's own padding (Section 3.1 of the
        /// Ethernet convergence layer draft); 42 is enough behind an
        /// 802.1Q tag, and 46 is correct either way.  On any datagram link
        /// a floor also hides the size of small messages, such as a lone
        /// Transfer Cancel, from an observer, at the cost of the padding;
        /// [`LinkFraming::FixedSize`] hides every PDU's size.
        ///
        /// A floor above the [`PduSize`] pads every PDU to the `PduSize`.
        /// A bare bundle frame ([`BundleFraming::Bare`]) cannot be padded,
        /// since the padding would be read as bundle bytes, so a bundle
        /// shorter than the floor is sent as a Bundle Message and padded.
        #[cfg_attr(feature = "serde", serde(default))]
        min_pdu_len: usize,
    },
}

impl LinkFraming {
    /// Variable-length PDUs framing a fitting bundle as `bundle_framing`
    /// says, with no `min_pdu_len` floor.
    pub const fn variable(bundle_framing: BundleFraming) -> Self {
        Self::Variable {
            bundle_framing,
            min_pdu_len: 0,
        }
    }
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
    /// Bundle Message.  It also frames a bundle shorter than the
    /// `min_pdu_len` floor as a Bundle Message, which can be padded.
    ///
    /// **Padding links.** A bare frame carries nothing that tells the
    /// receiver where the bundle ends, so a link that pads frames to a
    /// minimum size (Ethernet's 46-octet minimum payload, for instance)
    /// delivers the padding as bundle bytes.  Setting `min_pdu_len` to the
    /// link's minimum keeps this sender's bare frames clear of it.  A
    /// receiver cannot rely on that, since its peer may be another
    /// implementation, so it still needs to delimit a bare bundle itself
    /// (see [`BundleExtent`](crate::codec::BundleExtent) for this crate's
    /// receive side).  A link that pads to a fixed size should use
    /// [`LinkFraming::FixedSize`].  When in doubt, use
    /// [`BundleFraming::Message`].
    Bare,
}

/// When a [`Sender`] cuts a segment whose bytes are still being pushed
/// (see [`Sender::push`]).
///
/// A segment carries what remains of the bundle up to a full segment, or
/// fills the room left in a PDU if that is at least half a segment.  The
/// policy decides what happens when fewer of those bytes have been pushed.
/// Under either policy every segment but the last carries at least half a
/// full segment, so a bundle's segment count stays within twice its
/// full-size count.  A bundle given whole to [`Sender::enqueue`] is cut the
/// same way under both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "kebab-case")
)]
pub enum SegmentCutStrategy {
    /// Wait until all of the segment's bytes are pushed.  A slow producer
    /// does not multiply the segment count, and segments are as long as
    /// the PDU allows.
    #[default]
    Full,
    /// Cut once at least half a full segment's bytes are pushed, carrying
    /// what has been pushed.  A producer whose chunks are not a multiple of
    /// the segment size, say one and a half PDUs, then has each chunk sent
    /// as one full segment and one shorter one, and released, without
    /// waiting for the next chunk.  The cost is more segments, PDUs that go
    /// out part-filled, and segments short enough that a receiver may copy
    /// rather than share them.
    Half,
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
    /// The send queue's backpressure threshold, in bytes.  Default: 1 MiB.
    pub send_queue_high_watermark: SendQueueHighWatermark,
    /// The link's framing discipline.  Default: fixed-size frames.
    pub link_framing: LinkFraming,
    /// When a segment of a bundle still being pushed is cut.  Default:
    /// once all of its bytes are pushed.
    pub segment_cut_strategy: SegmentCutStrategy,
}
