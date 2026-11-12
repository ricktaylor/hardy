//! Counters of what a sender has done since it was created.

/// Running totals kept by a [`Sender`](super::Sender), none of which the
/// values it returns would tell a caller.  The totals only grow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SenderStats {
    /// PDUs packed, through any output.
    pub pdus_packed: u64,
    /// PDU buffers allocated: one for each PDU packed by `next_pdu` or
    /// `next_pdu_into`, and none for a call that finds nothing to pack,
    /// for a bare bundle frame, which is its own buffer, or for a PDU
    /// packed into memory the caller supplies.
    pub pdu_buffers_allocated: u64,
    /// Transfer numbers taken, one for each segmented bundle admitted.
    pub transfers_allocated: u64,
    /// Bundles cancelled while still outstanding.
    pub bundles_cancelled: u64,
    /// Transfer Cancel messages queued, one for each cancelled transfer
    /// that had already emitted a segment.
    pub transfer_cancels_queued: u64,
}
