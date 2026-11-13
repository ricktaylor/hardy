//! Counters of what a sender has done since it was created.

/// Running totals kept by a [`Sender`](super::Sender) since it was
/// created, as [`Sender::stats`](super::Sender::stats) returns them.  The
/// totals only grow, and a value is a snapshot: the difference between two
/// is what the sender did in between.
///
/// Each total comes out the same whichever output the PDUs were packed
/// through.  A refused `enqueue` or `begin` is not counted, since the
/// caller holds the error and the refusal leaves the sender as it was.
///
/// The byte and segment totals separate what a bundle needs sent once from
/// what goes onto the link.  [`Self::bundle_bytes_packed`],
/// [`Self::segments_packed`], and [`Self::flush_segments_packed`] count
/// each bundle byte and each segment once, when it is first packed;
/// [`Self::pdu_bytes_packed`] and [`Self::padding_bytes_packed`] count
/// every byte packed.  The two agree while the sender packs each message
/// once.  A further copy of a message (Section 6), or the repair symbols
/// of an FEC transfer, would add to the PDU bytes and to totals of their
/// own, not to the bundle bytes or the segments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SenderStats {
    pdus_packed: u64,
    pdu_bytes_packed: u64,
    bundle_bytes_packed: u64,
    padding_bytes_packed: u64,
    segments_packed: u64,
    flush_segments_packed: u64,
    transfers_allocated: u64,
    bundles_cancelled: u64,
    transfer_cancels_queued: u64,
}

/// What the sender reports as it happens.  Each event moves every total
/// it bears on, so no call site chooses among the counters.
impl SenderStats {
    /// A PDU of `len` bytes was packed from messages, the last `padding`
    /// bytes of it padding.
    pub(super) fn pdu_packed(&mut self, len: usize, padding: usize) {
        self.pdus_packed += 1;
        self.pdu_bytes_packed += len as u64;
        self.padding_bytes_packed += padding as u64;
    }

    /// A bare bundle frame of `len` bytes went out as a PDU of its own:
    /// all of it bundle bytes, with no message around it and no padding.
    pub(super) fn bare_frame_packed(&mut self, len: usize) {
        self.pdus_packed += 1;
        self.pdu_bytes_packed += len as u64;
        self.bundle_bytes_packed += len as u64;
    }

    /// A Bundle Message carrying `data_len` bundle bytes was packed.
    pub(super) fn bundle_message_packed(&mut self, data_len: usize) {
        self.bundle_bytes_packed += data_len as u64;
    }

    /// The first copy of a Transfer Segment or Transfer End carrying
    /// `data_len` bundle bytes was packed, cut as the cut strategy cuts.
    pub(super) fn segment_packed(&mut self, data_len: usize) {
        self.segments_packed += 1;
        self.bundle_bytes_packed += data_len as u64;
    }

    /// [`Self::segment_packed`], for a segment that a flush cut.
    pub(super) fn segment_flushed(&mut self, data_len: usize) {
        self.segment_packed(data_len);
        self.flush_segments_packed += 1;
    }

    /// A segmented bundle was admitted and took a transfer number.
    pub(super) fn transfer_allocated(&mut self) {
        self.transfers_allocated += 1;
    }

    /// A bundle still outstanding was cancelled.
    pub(super) fn bundle_cancelled(&mut self) {
        self.bundles_cancelled += 1;
    }

    /// A Transfer Cancel was queued for a transfer with a segment packed.
    pub(super) fn transfer_cancel_queued(&mut self) {
        self.transfer_cancels_queued += 1;
    }
}

/// The totals.
impl SenderStats {
    /// PDUs packed, through any output.
    pub const fn pdus_packed(&self) -> u64 {
        self.pdus_packed
    }

    /// The bytes of every PDU packed: messages, padding, and bare bundle
    /// frames.
    pub const fn pdu_bytes_packed(&self) -> u64 {
        self.pdu_bytes_packed
    }

    /// Bundle bytes packed, in Bundle Messages, in Transfer Segment and
    /// Transfer End messages, and as bare bundle frames, each byte counted
    /// once.  Message headers, hint items, and padding are not bundle
    /// bytes.
    pub const fn bundle_bytes_packed(&self) -> u64 {
        self.bundle_bytes_packed
    }

    /// The bytes of the padding messages packed, headers included: what
    /// fills a PDU out to the PDU size under fixed-size framing, or to the
    /// link's shortest PDU under variable framing.
    pub const fn padding_bytes_packed(&self) -> u64 {
        self.padding_bytes_packed
    }

    /// Transfer Segment and Transfer End messages packed, each segment
    /// counted once.  A bundle sent as a Bundle Message or a bare frame
    /// adds none.
    pub const fn segments_packed(&self) -> u64 {
        self.segments_packed
    }

    /// Those of [`Self::segments_packed`] that a flush cut short of what
    /// the cut strategy waits for (see `NextPduOptions::flush`).
    pub const fn flush_segments_packed(&self) -> u64 {
        self.flush_segments_packed
    }

    /// Transfer numbers taken, one for each segmented bundle admitted.
    pub const fn transfers_allocated(&self) -> u64 {
        self.transfers_allocated
    }

    /// Bundles cancelled while still outstanding.
    pub const fn bundles_cancelled(&self) -> u64 {
        self.bundles_cancelled
    }

    /// Transfer Cancel messages queued, one for each cancelled transfer
    /// that had already had a segment packed.
    pub const fn transfer_cancels_queued(&self) -> u64 {
        self.transfer_cancels_queued
    }
}
