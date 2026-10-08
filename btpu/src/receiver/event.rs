//! What the receiver reports.

use bytes::Bytes;

#[cfg(doc)]
use super::{Delivery, MaxRetainedBytes, MaxSegments, MaxTransferSize, Receiver, SEGMENT_OVERHEAD};
#[cfg(all(doc, target_has_atomic = "ptr"))]
use crate::budget::RetentionBudget;
use crate::{
    codec::{Error, hint::Hints},
    transfer::TransferId,
};

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
    /// The caller gave up on the transfer with [`Receiver::refuse`].
    Refused,
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
    /// The message would have taken the [`RetentionBudget`] the receiver
    /// shares with others over its limit.  Handled as
    /// [`Self::ReceiverFull`] is: the transfer is closed and transfers
    /// already held are kept.  Reported first as `ReceiverFull` when both
    /// limits are exceeded.
    ///
    /// Reject reasons are local diagnostics, for events, logs, and metrics.
    /// A protocol that reports to a peer why its transfer was given up must
    /// send this and `ReceiverFull` as one reason the peer cannot tell
    /// apart, so that the peer cannot probe how full the link is.
    BudgetFull,
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

    /// Under [`Delivery::Streamed`], a segmented transfer's first bytes are
    /// ready: [`Self::TransferData`] events follow, then
    /// [`Self::TransferFinished`].
    ///
    /// Reported when the transfer's bytes from its start first include
    /// data, not when its first message arrives, so a consumer starting a
    /// bundle here has bytes to read at once.  A transfer that never
    /// receives segment 0 is never started.  Once started, a transfer ends
    /// with exactly one of [`Self::TransferFinished`],
    /// [`Self::TransferCancelled`], [`Self::TransferExpired`], or
    /// [`Self::TransferRejected`] for the same id, or silently by
    /// [`Receiver::reset`]; any but the first means the bytes already
    /// released are not a whole bundle.
    TransferStarted {
        /// The transfer, as the events that follow name it.
        id: TransferId,
        /// The transfer's hint items so far (see [`Self::Received`]), empty
        /// if it has none.  The events that follow carry the set again only
        /// when it changes.
        hints: Hints,
    },

    /// Under [`Delivery::Streamed`], the next segment of a started
    /// transfer, in order.  Segments holding no data are not reported.
    TransferData {
        /// The transfer.
        id: TransferId,
        /// The segment's bytes: a view of the PDU it arrived in, or a copy
        /// if it was held and short (see [`Receiver`]).
        data: Bytes,
        /// The transfer's full hint set if it has changed since the last
        /// event for this transfer, else `None`.  A consumer starts from
        /// the set [`Self::TransferStarted`] carried and keeps the latest
        /// `Some`.  A change carried by a message that released
        /// nothing is reported on the next event.
        hints: Option<Hints>,
    },

    /// Under [`Delivery::Streamed`], the last segment of a started
    /// transfer: the bundle is complete.  Later messages for the transfer
    /// are dropped as [`DropReason::Delivered`].
    TransferFinished {
        /// The transfer.
        id: TransferId,
        /// The final segment's bytes, empty if it held none.
        data: Bytes,
        /// As for [`Self::TransferData`].
        hints: Option<Hints>,
    },

    /// A transfer was cancelled by the sender.
    ///
    /// Also reported for a Cancel of an in-window number the receiver has
    /// seen nothing of: Section 5 counts every number in the window as in
    /// progress, and Section 8.4 has segments arriving after the Cancel
    /// discarded, so the number is remembered as cancelled either way.
    TransferCancelled {
        /// The cancelled transfer.
        id: TransferId,
    },

    /// A transfer was evicted from the window (incomplete).  One window
    /// advance can evict several; they are reported oldest first.
    TransferExpired {
        /// The expired transfer.
        id: TransferId,
    },

    /// A message was dropped without being applied to any transfer.
    /// Informational: the caller decides whether this matters (statistics,
    /// logging, or nothing at all).
    MessageDropped {
        /// The transfer the message named.
        transfer_number: u32,
        /// The transfer's id, as the receiver's other events for it name
        /// it, or `None` if the number is outside the receive window
        /// ([`DropReason::OutsideWindow`], [`DropReason::UnknownTransfer`]),
        /// where no id is assigned.
        id: Option<TransferId>,
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
        id: TransferId,
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
