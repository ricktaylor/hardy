//! One in-progress transfer and the segments it holds.

use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    vec::Vec,
};
use core::mem::{replace, take};

use bytes::{BufMut, Bytes, BytesMut};

#[cfg(doc)]
use super::{Delivery, MaxRetainedBytes, MaxTransferSize, ReceiverEvent};
use super::{HINT_OVERHEAD, RejectReason, SEGMENT_OVERHEAD, config::overhead_budget};
use crate::codec::hint::{HintItem, Hints};

/// Whether a transfer uses core segmentation or FEC, and for FEC the
/// configuration its first message named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Core,
    Fec(FecConfig),
}

/// The part of an FEC transfer's configuration visible without an FEC
/// scheme: which message form it uses and the identifier that form carries.
/// A pre-agreed FEC Instance ID and an explicit FEC Encoding ID name
/// different things, so the two never compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecConfig {
    PreAgreed { instance_id: u8 },
    Explicit { encoding_id: u8 },
}

/// The content of a Transfer Segment or Transfer End, annotated with what
/// the receiver needs to store it.
pub struct CoreSegment {
    pub index: u32,
    pub data: Bytes,
    /// A Transfer End, whose index is the transfer's final index `N`.
    pub end: bool,
    /// The length of the PDU `data` was decoded from, if any, for the copy
    /// rule in [`retain_segment`].
    pub pdu_len: Option<usize>,
}

impl CoreSegment {
    /// Whether the segment contradicts the sequence the transfer has
    /// already established (Section 4: one transfer is segments `0..=N`).
    /// Such a segment would make completion unsatisfiable forever.
    pub fn conflicts(&self, transfer: &InProgressTransfer) -> bool {
        if self.end {
            // One transfer has exactly one final segment: a second End
            // disagreeing with the recorded final index, or an End claiming
            // a final index below a segment already seen.  A repeated
            // identical End is normal repetition and stays idempotent.
            transfer
                .final_segment_index
                .is_some_and(|n| n != self.index)
                || transfer
                    .highest_index
                    .is_some_and(|highest| highest > self.index)
        } else {
            // A segment beyond the established final index would leave the
            // highest index above N.
            transfer.final_segment_index.is_some_and(|n| self.index > n)
        }
    }

    /// Whether the segment adds nothing to the transfer: its index is
    /// already stored or released and, for an End, already recorded as the
    /// final index.  An End at the index of a stored or released Segment
    /// still records `N`.
    pub fn is_duplicate(&self, transfer: &InProgressTransfer) -> bool {
        transfer.has_segment(self.index)
            && (!self.end || transfer.final_segment_index == Some(self.index))
    }
}

/// The bookkeeping charge for `segments` segments: [`SEGMENT_OVERHEAD`]
/// each.
pub fn segment_overhead(segments: usize) -> usize {
    segments.saturating_mul(SEGMENT_OVERHEAD)
}

/// What a transfer's retained hints count against the limits:
/// [`HINT_OVERHEAD`] plus the value length for each item held as
/// [`HintItem::Unknown`].  A well-formed Bundle Length is held inline and
/// charged nothing.
pub fn hint_charge(hints: &Hints) -> usize {
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
pub fn own_hint(hint: HintItem) -> HintItem {
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
pub fn retain_segment(data: Bytes, pdu_len: Option<usize>) -> (Bytes, usize) {
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

/// A segment held by its transfer.
pub struct Stored {
    pub data: Bytes,
    /// The memory `data` keeps alive, as [`retain_segment`] reported it.
    pub held: usize,
    /// Counted in [`InProgressTransfer::retained_segments`].
    pub retained: bool,
}

/// What [`InProgressTransfer::release_next`] found.
pub enum Release {
    /// The next segment in order, removed from the transfer; `last` if its
    /// index is the final index.
    Segment { data: Bytes, last: bool },
    /// Every segment through the final index was released earlier: the
    /// End named the index of a segment already released.
    Complete,
    /// The next segment has not arrived, or the final index is unknown.
    Waiting,
}

/// What [`InProgressTransfer::next_step`] released.
pub enum Step {
    /// The next segment has not arrived, or the final index is unknown.
    Waiting,
    /// The transfer ended without a data byte.
    Empty,
    /// The next contiguous bytes: the transfer's first event if `start`
    /// holds its full hint set, its last if `last`, with `hints` if they
    /// changed since last taken.
    Out {
        start: Option<Hints>,
        data: Bytes,
        last: bool,
        hints: Option<Hints>,
    },
}

pub struct InProgressTransfer {
    pub kind: TransferKind,
    /// Segments arrived and not released, by index.
    pub segments: BTreeMap<u32, Stored>,
    pub final_segment_index: Option<u32>,
    /// The highest segment index stored or released.
    pub highest_index: Option<u32>,
    /// Segments released under [`Delivery::Streamed`], which are indices
    /// `0..released`.  Wider than an index, since releasing index
    /// `u32::MAX` makes it 2^32.
    pub released: u64,
    /// [`ReceiverEvent::TransferStarted`] has been reported.
    pub started: bool,
    pub hints: Hints,
    /// `hint_charge(&hints)`, kept in step by [`Self::apply_hints`].
    pub hints_charge: usize,
    /// `hints` changed since it was last reported.
    pub hints_changed: bool,
    /// Segment bytes received so far, including a segment refused for
    /// exceeding the segment limit and segments released: the bundle's
    /// length as far as it is known, compared against the
    /// [`MaxTransferSize`].
    pub data_bytes: usize,
    /// The memory the stored segments keep alive, the sum of their
    /// [`Stored::held`].
    pub held_bytes: usize,
    /// The stored segments that are [`Stored::retained`]: all of them but
    /// a segment released before the call that stored it returns.
    pub retained_segments: usize,
    /// A new segment arrived with the segment limit already reached, and
    /// was not stored.
    pub over_segment_limit: bool,
}

impl InProgressTransfer {
    pub fn new(kind: TransferKind) -> Self {
        Self {
            kind,
            segments: BTreeMap::new(),
            final_segment_index: None,
            highest_index: None,
            released: 0,
            started: false,
            hints: Hints::new(),
            hints_charge: 0,
            hints_changed: false,
            data_bytes: 0,
            held_bytes: 0,
            retained_segments: 0,
            over_segment_limit: false,
        }
    }

    /// Whether the segment at `index` is stored or was released.
    fn has_segment(&self, index: u32) -> bool {
        u64::from(index) < self.released || self.segments.contains_key(&index)
    }

    /// The distinct segments the transfer has had, stored or released.
    fn distinct_segments(&self) -> u64 {
        // usize is at most 64 bits on every supported target.
        self.released + self.segments.len() as u64
    }

    /// Whether `index` is the next segment to release under
    /// [`Delivery::Streamed`].
    pub fn is_next(&self, index: u32) -> bool {
        u64::from(index) == self.released
    }

    /// Insert a segment unless its index is already stored or released.
    /// Repeats are dropped before this; an occupied index here is an End
    /// naming a stored or released Segment's index as final, which keeps
    /// the first copy.
    ///
    /// With `retain`, the segment is detached from its PDU per
    /// [`retain_segment`] and charged what it keeps alive.  Without, it is
    /// stored as given and charged nothing, not even [`SEGMENT_OVERHEAD`],
    /// for a segment released before the call that stored it returns.
    ///
    /// With a `segment_limit`, a new segment that would exceed it is not
    /// stored but its bytes are still counted, so [`Self::exceeds`] can
    /// prefer [`RejectReason::TooLarge`] when the bundle is over both limits.
    pub fn insert_segment(
        &mut self,
        index: u32,
        data: Bytes,
        pdu_len: Option<usize>,
        retain: bool,
        segment_limit: Option<u64>,
    ) {
        if u64::from(index) < self.released {
            return;
        }
        let distinct = self.distinct_segments();
        let Entry::Vacant(e) = self.segments.entry(index) else {
            return;
        };
        self.data_bytes = self.data_bytes.saturating_add(data.len());
        if segment_limit.is_some_and(|limit| distinct >= limit) {
            self.over_segment_limit = true;
            return;
        }
        let (data, held) = if retain {
            retain_segment(data, pdu_len)
        } else {
            (data, 0)
        };
        self.held_bytes = self.held_bytes.saturating_add(held);
        self.retained_segments += usize::from(retain);
        self.highest_index = self.highest_index.max(Some(index));
        e.insert(Stored {
            data,
            held,
            retained: retain,
        });
    }

    /// Remove the next segment in order, if it has arrived, and count it
    /// released.
    fn release_next(&mut self) -> Release {
        let released = self.released;
        if let Some(entry) = self.segments.first_entry()
            && u64::from(*entry.key()) == released
        {
            let (index, stored) = entry.remove_entry();
            self.held_bytes = self.held_bytes.saturating_sub(stored.held);
            self.retained_segments -= usize::from(stored.retained);
            self.released += 1;
            return Release::Segment {
                data: stored.data,
                last: self.final_segment_index == Some(index),
            };
        }
        if self
            .final_segment_index
            .is_some_and(|n| u64::from(n) < self.released)
        {
            Release::Complete
        } else {
            Release::Waiting
        }
    }

    /// Release the next contiguous bytes under [`Delivery::Streamed`], as
    /// the event they make.
    pub fn next_step(&mut self) -> Step {
        loop {
            let (data, last) = match self.release_next() {
                Release::Segment { data, last } => (data, last),
                Release::Complete => (Bytes::new(), true),
                Release::Waiting => return Step::Waiting,
            };
            if data.is_empty() && !last {
                continue;
            }
            // The size limits were enforced on every insert.  Zero bytes
            // cannot be a valid bundle, as for an empty Bundle Message
            // (Section 8.1); a transfer with no data was never started.
            if last && self.data_bytes == 0 {
                return Step::Empty;
            }
            // The start reports the full set, so no change is pending.
            let start = (!replace(&mut self.started, true)).then(|| {
                self.hints_changed = false;
                self.hints.clone()
            });
            return Step::Out {
                start,
                data,
                last,
                hints: self.take_hints(),
            };
        }
    }

    /// The full hint set if it changed since last taken, else `None`.
    fn take_hints(&mut self) -> Option<Hints> {
        take(&mut self.hints_changed).then(|| self.hints.clone())
    }

    /// Which draft-ietf-dtn-btpu-fec rule a message of `kind` breaks on
    /// this transfer, if any: mixing core and FEC messages (Section 3.2) is
    /// [`RejectReason::FecCoreMixing`], and changing the FEC configuration
    /// mid-transfer (Sections 3 and 3.1) is
    /// [`RejectReason::FecConfigurationChanged`].  Either MUST cancel the
    /// transfer.
    pub fn kind_mismatch(&self, kind: TransferKind) -> Option<RejectReason> {
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
    /// [`SEGMENT_OVERHEAD`] per distinct segment, stored or released, when
    /// there is no `segment_limit`, so that a transfer passes or fails
    /// these rules the same way under either [`Delivery`].
    pub fn exceeds(&self, max: usize, segment_limit: Option<u64>) -> Option<RejectReason> {
        let segments = match segment_limit {
            Some(_) => 0,
            None => {
                segment_overhead(usize::try_from(self.distinct_segments()).unwrap_or(usize::MAX))
            }
        };
        if self.data_bytes > max || self.hints.bundle_length().is_some_and(|h| h > max as u64) {
            Some(RejectReason::TooLarge)
        } else if self.over_segment_limit
            || segments.saturating_add(self.hints_charge) > overhead_budget(max)
        {
            Some(RejectReason::TooFragmented)
        } else {
            None
        }
    }

    /// What the transfer counts against the receiver's [`MaxRetainedBytes`]:
    /// the memory its retained segments keep alive, [`SEGMENT_OVERHEAD`] per
    /// retained segment whatever the segment limit, and its hints.  A
    /// segment released in the call that stored it, like one released
    /// earlier, is not held, so not charged.
    pub fn charge(&self) -> usize {
        self.held_bytes
            .saturating_add(segment_overhead(self.retained_segments))
            .saturating_add(self.hints_charge)
    }

    /// Record hints from a message, keeping the latest value per hint type,
    /// and note whether the set changed.
    pub fn apply_hints(&mut self, hints: Vec<HintItem>) {
        let mut changed = false;
        for hint in hints {
            if !self.hints.contains(&hint) {
                self.hints.insert(own_hint(hint));
                changed = true;
            }
        }
        if changed {
            self.hints_charge = hint_charge(&self.hints);
            self.hints_changed = true;
        }
    }

    /// Check whether all segments 0..=N have been received.
    pub fn is_complete(&self) -> bool {
        let Some(n) = self.final_segment_index else {
            return false;
        };
        // `n + 1` distinct indices, the greatest `n`, can only be 0..=n.
        // Counted in u64: `n` is wire-supplied, so `n + 1` overflows u32
        // when a hostile End claims a final index of u32::MAX.
        self.distinct_segments() == u64::from(n) + 1 && self.highest_index == Some(n)
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
    pub fn reassemble(&self) -> Bytes {
        if let Some((_, only)) = self.segments.first_key_value()
            && self.segments.len() == 1
        {
            return only.data.clone();
        }
        let total = self.segments.values().map(|s| s.data.len()).sum();
        let mut buf = BytesMut::with_capacity(total);
        for stored in self.segments.values() {
            buf.put_slice(&stored.data);
        }
        buf.freeze()
    }
}
