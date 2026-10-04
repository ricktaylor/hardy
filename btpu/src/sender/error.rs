//! Errors from queuing bundles.

#[cfg(doc)]
use bytes::Bytes;

#[cfg(doc)]
use super::{BundleFraming, Sender};
// Aliased: this module has its own `Error`.
use crate::transfer::Error as TransferError;

/// Shorthand for results whose error is [`enum@Error`] unless stated.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors from queuing bundles for transmission.
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
    /// The count allows for segments cut short to fill the tail of a PDU,
    /// which carry at least half of a full segment, so a bundle is refused
    /// once twice its full-size segment count would not fit.
    ///
    /// Theoretical in practice: a bundle over 4 GiB needs a PDU of at least
    /// 23 bytes to segment (see [`Self::PduTooSmall`]), whose later
    /// segments carry at least 11 bytes each, or 6 when cut short, so only
    /// a bundle of more than 24 GiB in memory can reach it, and none can on
    /// a 32-bit target.
    #[error("Bundle of {len} bytes needs more than 2^32 segments at PDU size {pdu_size}")]
    TooManySegments {
        /// The bundle's length in bytes.
        len: usize,
        /// The configured PDU size.
        pdu_size: usize,
    },

    /// A [`Sender::push`] would take the bundle past the `total_len` given
    /// to [`Sender::begin`].  Nothing is pushed.
    #[error("Pushing {chunk} bytes would overrun the bundle: {pushed} of {total_len} bytes pushed")]
    Overrun {
        /// The bundle's length, as given to [`Sender::begin`].
        total_len: usize,
        /// The bytes pushed before the refused chunk.
        pushed: usize,
        /// The length of the refused chunk.
        chunk: usize,
    },

    /// [`Sender::finish`] was called before the bundle's last byte was
    /// pushed.  The bundle is cancelled, as [`Sender::cancel`] would.
    #[error("Bundle finished short: {pushed} of {total_len} bytes pushed")]
    Underrun {
        /// The bundle's length, as given to [`Sender::begin`].
        total_len: usize,
        /// The bytes pushed.
        pushed: usize,
    },

    /// The handle names no bundle in progress: the bundle was cancelled by
    /// its ID.
    #[error("No bundle in progress for this send handle")]
    NotInProgress,

    /// The handle was issued by another sender.  A bug in the caller:
    /// each handle belongs to the [`Sender`] whose [`Sender::begin`]
    /// returned it.  Nothing is changed.
    #[error("Send handle was issued by another sender")]
    ForeignHandle,

    /// Counting the bytes would take [`Sender::queued_bytes`] past
    /// `u64::MAX`.  Nothing is queued or pushed.
    ///
    /// Unreachable in practice: each queued [`Bytes`] is at most
    /// `isize::MAX` bytes, so only clones of one buffer queued billions of
    /// times can reach it, and on a 32-bit target the queue entries alone
    /// would not fit in memory first.
    #[error("Queued byte count would overflow")]
    QueuedBytesOverflow,

    /// The first chunk of a bundle [`Sender::begin`] chose to send as a
    /// bare bundle frame ([`BundleFraming::Bare`]) does not start with a
    /// bundle-reserved byte, so a receiver would not take it for a bundle.
    /// Nothing is pushed.  `begin` cannot see the first byte, so it
    /// assumes a bundle; [`Sender::enqueue`] can, and frames such data as
    /// a Bundle Message instead.
    #[error("Bare bundle frame does not start with a bundle-reserved byte")]
    NotABundle,
}
