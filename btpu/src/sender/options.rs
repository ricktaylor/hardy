//! Per-bundle and per-PDU options.

use bytes::Bytes;

#[cfg(doc)]
use super::{SegmentCutStrategy, Sender, SenderConfig};
#[cfg(doc)]
use crate::codec::hint::HintType;
use crate::codec::hint::Hints;

/// Options for [`Sender::enqueue`] and [`Sender::begin`].
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

/// Options for [`Sender::next_pdu_with`] and [`Sender::next_pdu_into_with`]: how one
/// PDU is packed.  Policy that holds for every PDU belongs in
/// [`SenderConfig`].
///
/// `Default`-constructible; `NextPduOptions::default()` packs only what is
/// ready to send, as [`Sender::next_pdu`] and the `tower` `Stream` impl do.
// Not `#[non_exhaustive]`: a new field is a new packing policy, and the
// compile break on callers' struct literals is the useful checklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NextPduOptions {
    /// Also send what bundles waiting on their producers have buffered.
    ///
    /// The PDU is packed as usual, then each segmented bundle whose next
    /// segment waits on bytes not yet pushed (see [`SegmentCutStrategy`]),
    /// in queue order, cuts a segment of what it has buffered into the room
    /// left, at least one data byte.  A PDU that would otherwise be empty
    /// carries those segments alone.  The packing stops at the first queued
    /// message that is not part of such a bundle, as it does without a
    /// flush, so bundles are not reordered.
    ///
    /// The sender has no clock, so it never cuts short of the
    /// [`SegmentCutStrategy`] on its own.  A CLA that has one asks for a
    /// flush when its producer has been quiet for long enough, as TCP's
    /// cork timer does, or when the link offers a transmission slot that
    /// would otherwise carry only padding.
    ///
    /// A flushed segment may be far shorter than half a segment, so
    /// frequent flushes raise a bundle's segment count, and with it what
    /// the receiver must allow (see `MaxSegments` in the receiver).  A cut
    /// is refused if the rest of the bundle, segmented normally, could then
    /// need the segment index `u32::MAX`.  A bundle that fits one PDU is
    /// queued only once fully pushed, so it is never flushed.
    pub flush: bool,
}
