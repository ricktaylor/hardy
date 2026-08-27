use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    vec,
    vec::Vec,
};
use core::{num::NonZeroUsize, ops::ControlFlow};

use bytes::{BufMut, Bytes, BytesMut};

use crate::{
    codec::{
        self,
        hint::{BUNDLE_LENGTH, HintItem},
        message::{Message, TransferEndMessage, TransferSegmentMessage},
    },
    transfer::{TransferValidity, TransferWindow, WindowSize},
};

/// Errors from receiver configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The maximum bundle size is zero.
    #[error("Invalid max bundle size {0} (must be non-zero)")]
    InvalidMaxBundleSize(usize),
}

/// A validated cap on the size of bundle a [`Receiver`] accepts, in bytes
/// (non-zero).
///
/// Construct via [`TryFrom<usize>`], which enforces the bound at the edge;
/// every consumer of a `MaxBundleSize` can then rely on it.  There is no
/// "unlimited" value: a receiver reassembles bundles in memory, so an
/// unbounded cap hands a remote peer a memory-exhaustion lever.  Where no
/// meaningful limit exists, say so explicitly with `usize::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct MaxBundleSize(NonZeroUsize);

impl MaxBundleSize {
    /// Returns the cap as a plain integer.
    pub fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for MaxBundleSize {
    /// 1 GiB, matching the Hardy TCPCLv4 transfer-MRU default.
    fn default() -> Self {
        Self(NonZeroUsize::new(0x4000_0000).unwrap())
    }
}

impl TryFrom<usize> for MaxBundleSize {
    type Error = Error;

    fn try_from(v: usize) -> Result<Self, Self::Error> {
        NonZeroUsize::new(v)
            .map(Self)
            .ok_or(Error::InvalidMaxBundleSize(v))
    }
}

impl From<MaxBundleSize> for usize {
    fn from(m: MaxBundleSize) -> usize {
        m.0.get()
    }
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
    /// The transfer was rejected for exceeding the maximum bundle size
    /// configured in [`Receiver::new`].
    TooLarge,
    /// The message's transfer mode (core segmentation vs FEC) conflicts with
    /// the mode the transfer is already using; the two must not be mixed
    /// within one transfer.
    FecCoreMixing,
    /// A Transfer Segment carried zero octets of data (Section 8.2 SHOULD
    /// NOT).  Not stored: an index flood of empty segments would otherwise
    /// grow reassembly state without ever counting toward the bundle-size
    /// cap.
    EmptySegment,
    /// The message's segment index contradicts the transfer's established
    /// segment sequence (Section 4: segments run 0..=N with exactly one
    /// final index): a second End disagreeing with the recorded final index,
    /// an End claiming a final index below a segment already seen, or a
    /// segment beyond the final index.  Applying it would make completion
    /// permanently unsatisfiable.
    SegmentIndexConflict,
}

/// Events emitted by the receiver for the calling CLA to act on.
///
/// Deliberately exhaustive: values are produced by this crate, never decoded
/// from the wire, and a consumer that acts per-variant should get a compile
/// error when one is added.
#[derive(Debug)]
pub enum ReceiverEvent {
    /// A complete bundle has been reassembled (or received as a Bundle
    /// message).
    BundleReceived {
        data: Bytes,
        /// The transfer's hint items (Section 7.2), including hint types
        /// this implementation does not recognise — extension metadata (a
        /// correlator, say) reaches the caller without an API change.  One
        /// item per hint type, in ascending hint-type order: hints are
        /// transfer-scoped and repeatable, and a later value supersedes an
        /// earlier one of the same type, whether the repeat is in a later
        /// message of a transfer or within a single Bundle message.
        hints: Vec<HintItem>,
    },

    /// A transfer was cancelled by the sender.
    TransferCancelled { transfer_number: u32 },

    /// A transfer was evicted from the window (incomplete).
    TransferExpired { transfer_number: u32 },

    /// A message was dropped without being applied to any transfer.
    /// Informational: the caller decides whether this matters (statistics,
    /// logging, or nothing at all).
    MessageDropped {
        transfer_number: u32,
        reason: DropReason,
    },

    /// An in-progress transfer was rejected by local policy: its accumulated
    /// segment data, or the sender's Bundle Length hint, exceeds the maximum
    /// bundle size configured in [`Receiver::new`].
    TransferRejected { transfer_number: u32 },

    /// An unsegmented Bundle message was rejected by local policy: its
    /// content exceeds the maximum bundle size configured in
    /// [`Receiver::new`].  The counterpart of [`Self::TransferRejected`] for
    /// bundles that never had a transfer number.
    BundleRejected {
        /// The rejected content length in bytes.
        len: usize,
    },

    /// One message could not be decoded and was skipped; processing
    /// continued at the next message boundary given by the Section 7 header
    /// length.
    MalformedMessage { error: codec::Error },

    /// The PDU could not be walked further — no message boundary could be
    /// determined — and the remainder was discarded.  Always the final event
    /// of its PDU.
    MalformedPdu { error: codec::Error },
}

/// Whether a transfer uses core segmentation or FEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferKind {
    Core,
    Fec,
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
fn dedup_hints(hints: Vec<HintItem>) -> Vec<HintItem> {
    let mut map = BTreeMap::new();
    fold_hints(&mut map, hints);
    map.into_values().collect()
}

struct InProgressTransfer {
    kind: TransferKind,
    segments: BTreeMap<u32, Bytes>,
    final_segment_index: Option<u32>,
    /// The transfer's hint items, keyed by hint type: hints are
    /// transfer-scoped and repeatable (Section 7.2), so a later message's
    /// value supersedes an earlier one.  Structurally bounded — at most 2^7
    /// types of at most 255 value bytes each — so no policy cap is needed.
    hints: BTreeMap<u8, HintItem>,
    /// Total bytes across accepted (non-duplicate) segments.
    received_bytes: usize,
}

impl InProgressTransfer {
    fn new(kind: TransferKind) -> Self {
        Self {
            kind,
            segments: BTreeMap::new(),
            final_segment_index: None,
            hints: BTreeMap::new(),
            received_bytes: 0,
        }
    }

    /// Insert a segment unless it is a duplicate (repetition support).
    fn insert_segment(&mut self, index: u32, data: Bytes) {
        if let Entry::Vacant(e) = self.segments.entry(index) {
            self.received_bytes += data.len();
            e.insert(data);
        }
    }

    /// Whether the transfer provably exceeds `max` bytes, either by data
    /// accumulated so far or by the sender's Bundle Length hint.
    fn exceeds(&self, max: usize) -> bool {
        self.received_bytes > max || self.bundle_length_hint().is_some_and(|h| h > max as u64)
    }

    /// The sender's Bundle Length hint, if one has been received.
    fn bundle_length_hint(&self) -> Option<u64> {
        match self.hints.get(&BUNDLE_LENGTH) {
            Some(HintItem::BundleLength(len)) => Some(*len),
            _ => None,
        }
    }

    /// Record hints from a message, keeping the latest value per hint type.
    fn apply_hints(&mut self, hints: &[HintItem]) {
        fold_hints(&mut self.hints, hints.iter().cloned());
    }

    /// Check whether all segments 0..=N have been received.
    fn is_complete(&self) -> bool {
        let Some(n) = self.final_segment_index else {
            return false;
        };
        // Count in u64: `n` is wire-supplied, so `n + 1` overflows u32 when a
        // hostile End claims a final index of u32::MAX.
        self.segments.len() as u64 == u64::from(n) + 1
            && self.segments.keys().next_back() == Some(&n)
            && self.segments.keys().next() == Some(&0)
    }

    /// Concatenate segments in order and return the reassembled bundle.
    ///
    /// A single-segment transfer hands back its lone [`Bytes`] — a refcount
    /// bump, no copy.  Multi-segment reassembly deliberately copies once
    /// into a contiguous buffer: the BPA parses bundles from contiguous
    /// bytes, and one copy per delivered bundle is cheap relative to the
    /// transfer itself.
    fn reassemble(&mut self) -> Bytes {
        if self.segments.len() == 1 {
            return self.segments.pop_first().expect("length checked").1;
        }
        let total: usize = self.segments.values().map(|s| s.len()).sum();
        let mut buf = BytesMut::with_capacity(total);
        for data in self.segments.values() {
            buf.put_slice(data);
        }
        buf.freeze()
    }
}

/// Manages inbound PDU processing, transfer window, and segment reassembly.
pub struct Receiver {
    max_bundle_size: MaxBundleSize,
    window: TransferWindow,
    transfers: BTreeMap<u32, InProgressTransfer>,
    /// Transfers abandoned before completion and why: cancelled by the sender
    /// (Section 4.2: repeated segments MUST NOT re-create them) or rejected by
    /// local policy (oversize).  Keys are always in-window (pruned by
    /// [`Self::expire_old_transfers`]), so the map is bounded by the window
    /// size.
    abandoned: BTreeMap<u32, DropReason>,
}

impl Receiver {
    /// Create a new receiver.
    ///
    /// `max_bundle_size` bounds the bundles this receiver accepts, in bytes:
    /// transfers that provably exceed it are rejected with
    /// [`ReceiverEvent::TransferRejected`], and oversized Bundle messages
    /// with [`ReceiverEvent::BundleRejected`].
    pub fn new(window_size: WindowSize, max_bundle_size: MaxBundleSize) -> Self {
        Self {
            max_bundle_size,
            window: TransferWindow::new(window_size),
            transfers: BTreeMap::new(),
            abandoned: BTreeMap::new(),
        }
    }

    /// Process a received convergence layer PDU.  Returns zero or more events.
    ///
    /// Infallible at the PDU level: every framing and semantic fault is
    /// expressed as an event ([`ReceiverEvent::MalformedMessage`],
    /// [`ReceiverEvent::MalformedPdu`], [`ReceiverEvent::MessageDropped`],
    /// ...) alongside whatever the well-formed messages produced — a fault
    /// late in a PDU never discards the events of the prefix before it.
    ///
    /// Taking `pdu` by value (rather than `&[u8]`) lets the codec extract
    /// message data as zero-copy [`Bytes`] views into the original buffer.
    pub fn receive_pdu(&mut self, pdu: Bytes) -> Vec<ReceiverEvent> {
        let mut events = Vec::new();
        let mut messages = codec::decode_pdu(pdu);
        while let Some(item) = messages.next() {
            match item {
                Ok(msg) => events.append(&mut self.process_message(msg)),
                Err(error) if messages.is_exhausted() => {
                    events.push(ReceiverEvent::MalformedPdu { error });
                }
                Err(error) => {
                    events.push(ReceiverEvent::MalformedMessage { error });
                }
            }
        }
        events
    }

    /// Process a single decoded message.
    pub fn process_message(&mut self, message: Message) -> Vec<ReceiverEvent> {
        match message {
            Message::IndefinitePadding | Message::DefinitePadding { .. } => vec![],

            Message::Bundle { data, hints } => {
                if data.len() > self.max_bundle_size.get() {
                    return vec![ReceiverEvent::BundleRejected { len: data.len() }];
                }
                vec![ReceiverEvent::BundleReceived {
                    data,
                    hints: dedup_hints(hints),
                }]
            }

            Message::TransferSegment(m) => self.process_transfer_segment(m),

            Message::TransferEnd(m) => self.process_transfer_end(m),

            Message::TransferCancel { transfer_number } => {
                self.process_transfer_cancel(transfer_number)
            }

            // FEC messages are tracked but not decoded (no FEC scheme registered).
            // They are stored with TransferKind::Fec to detect mixing.
            Message::PreAgreedFecSource(m) => {
                self.process_fec_message(m.transfer_number, TransferKind::Fec, &m.hints)
            }
            Message::ExplicitFecSource(m) => {
                self.process_fec_message(m.transfer_number, TransferKind::Fec, &m.hints)
            }
            Message::PreAgreedFecRepair(m) => {
                self.process_fec_message(m.transfer_number, TransferKind::Fec, &m.hints)
            }
            Message::ExplicitFecRepair(m) => {
                self.process_fec_message(m.transfer_number, TransferKind::Fec, &m.hints)
            }

            Message::Unknown { .. } => vec![],
        }
    }

    // -- transfer processing ------------------------------------------------

    /// Admission check: is `transfer_number` eligible for processing at all?
    /// It must be inside the receive window and not previously abandoned
    /// (cancelled or rejected).
    ///
    /// Any events produced (window-advance expiries, drop reports) are pushed
    /// onto `events`.  Returns [`ControlFlow::Break`] when the message must
    /// not be applied.  Both drop cases are expected traffic, not faults, so
    /// they surface as [`ReceiverEvent::MessageDropped`] rather than
    /// [`Error`]s.
    fn gate_admission(
        &mut self,
        transfer_number: u32,
        events: &mut Vec<ReceiverEvent>,
    ) -> ControlFlow<()> {
        match self.window.process(transfer_number) {
            TransferValidity::OutsideWindow => {
                events.push(ReceiverEvent::MessageDropped {
                    transfer_number,
                    reason: DropReason::OutsideWindow,
                });
                return ControlFlow::Break(());
            }
            TransferValidity::New => {
                events.append(&mut self.expire_old_transfers());
            }
            TransferValidity::InProgress => {}
        }

        // A repeated message for an abandoned transfer MUST NOT re-create it
        // (Section 4.2 for cancelled; same trap applies to locally rejected
        // transfers).  Checked after the window (abandoned traffic is still
        // window-relevant) but before any transfer entry is inserted.
        if let Some(&reason) = self.abandoned.get(&transfer_number) {
            events.push(ReceiverEvent::MessageDropped {
                transfer_number,
                reason,
            });
            return ControlFlow::Break(());
        }

        ControlFlow::Continue(())
    }

    /// Enforce the configured maximum bundle size on an in-progress transfer.
    ///
    /// Runs after each segment insert: rejects as soon as the accumulated
    /// bytes exceed the limit, or earlier still if the sender's Bundle Length
    /// hint already promises an oversized bundle.  On rejection the transfer
    /// is dropped, recorded in `abandoned` (so repeated segments cannot
    /// re-create it), and reported via [`ReceiverEvent::TransferRejected`].
    fn gate_oversize(
        &mut self,
        transfer_number: u32,
        events: &mut Vec<ReceiverEvent>,
    ) -> ControlFlow<()> {
        let oversized = self
            .transfers
            .get(&transfer_number)
            .is_some_and(|t| t.exceeds(self.max_bundle_size.get()));
        if !oversized {
            return ControlFlow::Continue(());
        }

        self.transfers.remove(&transfer_number);
        self.abandoned.insert(transfer_number, DropReason::TooLarge);
        events.push(ReceiverEvent::TransferRejected { transfer_number });
        ControlFlow::Break(())
    }

    /// Shared pipeline for the two core transfer messages (Segment and
    /// End): admission → transfer-kind check → sequence-conflict check →
    /// state application → oversize gate → completion check.  One path for
    /// both keeps the gate ordering (and any future fix to it) in lockstep.
    ///
    /// `conflicts` is the per-message-type sequence-conflict predicate; when
    /// true the message is dropped as [`DropReason::SegmentIndexConflict`]
    /// with no state touched, hints included.  `apply` performs the
    /// message's state changes on the transfer.
    fn process_core_message(
        &mut self,
        transfer_number: u32,
        conflicts: impl FnOnce(&InProgressTransfer) -> bool,
        apply: impl FnOnce(&mut InProgressTransfer),
    ) -> Vec<ReceiverEvent> {
        let mut events = Vec::new();
        if self.gate_admission(transfer_number, &mut events).is_break() {
            return events;
        }

        let transfer = self
            .transfers
            .entry(transfer_number)
            .or_insert_with(|| InProgressTransfer::new(TransferKind::Core));

        if transfer.kind != TransferKind::Core {
            events.push(ReceiverEvent::MessageDropped {
                transfer_number,
                reason: DropReason::FecCoreMixing,
            });
            return events;
        }

        if conflicts(transfer) {
            events.push(ReceiverEvent::MessageDropped {
                transfer_number,
                reason: DropReason::SegmentIndexConflict,
            });
            return events;
        }

        apply(transfer);

        if self.gate_oversize(transfer_number, &mut events).is_break() {
            return events;
        }

        // A late segment may fill the final gap of a transfer whose End was
        // already received; check completeness after every apply, not just
        // after an End.
        self.complete_if_ready(transfer_number, &mut events);

        events
    }

    fn process_transfer_segment(&mut self, m: TransferSegmentMessage) -> Vec<ReceiverEvent> {
        // Section 8.2: segments SHOULD NOT be empty; they carry nothing to
        // reassemble, and storing them would let an index flood grow the
        // segment map without ever counting toward the bundle-size cap.
        // Dropped before any window or transfer state is touched.
        if m.data.is_empty() {
            return vec![ReceiverEvent::MessageDropped {
                transfer_number: m.transfer_number,
                reason: DropReason::EmptySegment,
            }];
        }

        self.process_core_message(
            m.transfer_number,
            // A segment beyond the established final index (Section 4: the
            // sequence is 0..=N) would leave the map's highest key above N
            // and make completion unsatisfiable forever.
            |t| t.final_segment_index.is_some_and(|n| m.segment_index > n),
            |t| {
                t.apply_hints(&m.hints);
                t.insert_segment(m.segment_index, m.data);
            },
        )
    }

    fn process_transfer_end(&mut self, m: TransferEndMessage) -> Vec<ReceiverEvent> {
        self.process_core_message(
            m.transfer_number,
            // One transfer has exactly one final segment (Section 4): a
            // second End disagreeing with the recorded final index, or an
            // End claiming a final index below a segment already seen, would
            // make completion unsatisfiable forever.  A repeated identical
            // End is normal repetition and stays idempotent.
            |t| {
                t.final_segment_index.is_some_and(|n| n != m.segment_index)
                    || t.segments
                        .keys()
                        .next_back()
                        .is_some_and(|&highest| highest > m.segment_index)
            },
            |t| {
                t.apply_hints(&m.hints);
                t.final_segment_index = Some(m.segment_index);
                // Section 8.3: an End SHOULD carry the final segment's data.
                // An empty End still fixes the final index (its
                // control-plane role) but stores nothing, keeping every map
                // entry at least one byte so the bundle-size cap bounds the
                // entry count too.
                if !m.data.is_empty() {
                    t.insert_segment(m.segment_index, m.data);
                }
            },
        )
    }

    /// If the transfer's segments are all present (and its final index is
    /// known), reassemble it, remove it from the window, and push a
    /// `BundleReceived` event.  A no-op otherwise.  Called after every segment
    /// or End insert so out-of-order completion is detected regardless of which
    /// message arrives last.
    fn complete_if_ready(&mut self, transfer_number: u32, events: &mut Vec<ReceiverEvent>) {
        let complete = self
            .transfers
            .get(&transfer_number)
            .is_some_and(InProgressTransfer::is_complete);
        if !complete {
            return;
        }

        // No max_bundle_size check needed here: gate_oversize enforces it on
        // every insert, so a transfer that reaches completion is within limit.
        let mut transfer = self.transfers.remove(&transfer_number).unwrap();
        events.push(ReceiverEvent::BundleReceived {
            data: transfer.reassemble(),
            hints: transfer.hints.into_values().collect(),
        });
    }

    fn process_transfer_cancel(&mut self, transfer_number: u32) -> Vec<ReceiverEvent> {
        // Section 8.4: a Cancel for an unknown transfer MUST be ignored.
        // "Ignored" includes side effects, so no gate_admission here: a Cancel
        // never advances the window.  In-progress transfers are in-window by
        // construction, so a valid Cancel has no window state to update.
        if self.transfers.remove(&transfer_number).is_some() {
            self.abandoned
                .insert(transfer_number, DropReason::Cancelled);
            return vec![ReceiverEvent::TransferCancelled { transfer_number }];
        }

        // Distinguish a repeated Cancel of an already-abandoned transfer
        // (idempotent, reported with the original abandon reason) from a
        // never-seen number.
        let reason = self
            .abandoned
            .get(&transfer_number)
            .copied()
            .unwrap_or(DropReason::UnknownTransfer);
        vec![ReceiverEvent::MessageDropped {
            transfer_number,
            reason,
        }]
    }

    fn process_fec_message(
        &mut self,
        transfer_number: u32,
        kind: TransferKind,
        hints: &[HintItem],
    ) -> Vec<ReceiverEvent> {
        let mut events = Vec::new();
        if self.gate_admission(transfer_number, &mut events).is_break() {
            return events;
        }

        let transfer = self
            .transfers
            .entry(transfer_number)
            .or_insert_with(|| InProgressTransfer::new(kind));

        if transfer.kind != kind {
            events.push(ReceiverEvent::MessageDropped {
                transfer_number,
                reason: DropReason::FecCoreMixing,
            });
            return events;
        }

        transfer.apply_hints(hints);
        // FEC reassembly requires a registered FecScheme; without one we just
        // track the transfer to maintain correct window state.

        events
    }

    // -- window expiry ------------------------------------------------------

    fn expire_old_transfers(&mut self) -> Vec<ReceiverEvent> {
        // Collect before removing: the lazy iterator borrows the key set.
        let expired: Vec<u32> = self
            .window
            .expired_transfers(self.transfers.keys().copied())
            .collect();
        let mut events = Vec::new();
        for t in expired {
            self.transfers.remove(&t);
            events.push(ReceiverEvent::TransferExpired { transfer_number: t });
        }

        // Prune the abandoned map the same way; this is what keeps it bounded
        // by the window size.  No events: these were already reported as
        // TransferCancelled / TransferRejected when they were abandoned.
        let expired: Vec<u32> = self
            .window
            .expired_transfers(self.abandoned.keys().copied())
            .collect();
        for t in expired {
            self.abandoned.remove(&t);
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(v: u16) -> WindowSize {
        WindowSize::try_from(v).unwrap()
    }

    fn make_receiver() -> Receiver {
        Receiver::new(WindowSize::default(), MaxBundleSize::default())
    }

    #[test]
    fn bundle_message_immediate() {
        let mut r = make_receiver();
        let msg = Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"hello"),
        };
        let events = r.process_message(msg);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ReceiverEvent::BundleReceived { data, .. } => assert_eq!(data.as_ref(), b"hello"),
            other => panic!("Expected BundleReceived, got {other:?}"),
        }
    }

    #[test]
    fn two_segment_transfer() {
        let mut r = make_receiver();

        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"hel"),
        }));
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived { .. }))
        );

        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"lo"),
        }));

        let received: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ReceiverEvent::BundleReceived { .. }))
            .collect();
        assert_eq!(received.len(), 1);
        match &received[0] {
            ReceiverEvent::BundleReceived { data, .. } => assert_eq!(data.as_ref(), b"hello"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn out_of_order_segments() {
        let mut r = make_receiver();

        // Send segment 1 (the End) first, then the late segment 0.  The End
        // fixes final_segment_index=1 but the transfer is not yet complete.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"ld"),
        }));
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived { .. }))
        );

        // The late segment 0 fills the final gap; completion must fire here,
        // on the segment insert, even though the End arrived earlier.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"wor"),
        }));

        let received: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ReceiverEvent::BundleReceived { .. }))
            .collect();
        assert_eq!(received.len(), 1);
        match &received[0] {
            ReceiverEvent::BundleReceived { data, .. } => assert_eq!(data.as_ref(), b"world"),
            _ => unreachable!(),
        }
        assert!(!r.transfers.contains_key(&0));
    }

    #[test]
    fn out_of_order_completes_on_end_recheck() {
        let mut r = make_receiver();

        // Segment 1 first
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"ld"),
        }));

        // Segment 0
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"wor"),
        }));

        // Transfer End with segment 2
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 2,
            hints: vec![],
            data: Bytes::from_static(b"!"),
        }));

        let received: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ReceiverEvent::BundleReceived { .. }))
            .collect();
        assert_eq!(received.len(), 1);
        match &received[0] {
            ReceiverEvent::BundleReceived { data, .. } => assert_eq!(data.as_ref(), b"world!"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn final_segment_index_of_u32_max_does_not_overflow() {
        let mut r = make_receiver();

        // A hostile End claiming u32::MAX as the final index must not
        // overflow the completeness check; the transfer just stays
        // incomplete (2^32 segments can never all be present).
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: u32::MAX,
            hints: vec![],
            data: Bytes::from_static(b"end"),
        }));
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived { .. }))
        );
        assert!(r.transfers.contains_key(&0));
    }

    #[test]
    fn conflicting_end_dropped_and_transfer_still_completes() {
        let mut r = make_receiver();

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"a"),
        }));
        r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 2,
            hints: vec![],
            data: Bytes::from_static(b"c"),
        }));

        // A second End disagreeing with the established final index is
        // dropped; accepting it would wedge completion forever.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"y"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::SegmentIndexConflict,
            }
        )));

        // The established index (and not the bogus End's data) still stands:
        // the missing middle segment completes the transfer.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"b"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data: b, .. } if b.as_ref() == b"abc"
        )));
    }

    #[test]
    fn segment_beyond_final_index_dropped() {
        let mut r = make_receiver();

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"a"),
        }));
        r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 2,
            hints: vec![],
            data: Bytes::from_static(b"c"),
        }));

        // A stray segment above the established final index is dropped;
        // storing it would keep the map's highest key above N forever.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 7,
            hints: vec![],
            data: Bytes::from_static(b"x"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::SegmentIndexConflict,
            }
        )));

        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"b"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data: b, .. } if b.as_ref() == b"abc"
        )));
    }

    #[test]
    fn end_below_seen_segment_dropped() {
        let mut r = make_receiver();

        for (i, d) in [(0u32, &b"a"[..]), (1, b"b"), (2, b"c")] {
            r.process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: i,
                hints: vec![],
                data: Bytes::copy_from_slice(d),
            }));
        }

        // An End claiming a final index below a segment already seen is
        // bogus (segments beyond the final index cannot exist); it must not
        // record a final index the map can never satisfy.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"y"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::SegmentIndexConflict,
            }
        )));

        // The genuine End still completes the transfer.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 2,
            hints: vec![],
            data: Bytes::from_static(b"c"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data: b, .. } if b.as_ref() == b"abc"
        )));
    }

    #[test]
    fn duplicate_segments_ignored() {
        let mut r = make_receiver();

        // Same segment twice
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"abc"),
        }));

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"SHOULD BE IGNORED"),
        }));

        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"def"),
        }));

        match &events.last().unwrap() {
            ReceiverEvent::BundleReceived { data, .. } => assert_eq!(data.as_ref(), b"abcdef"),
            other => panic!("Expected BundleReceived, got {other:?}"),
        }
    }

    #[test]
    fn transfer_cancel() {
        let mut r = make_receiver();

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 5,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"data"),
        }));

        let events = r.process_message(Message::TransferCancel { transfer_number: 5 });

        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReceiverEvent::TransferCancelled { transfer_number: 5 }))
        );
        assert!(!r.transfers.contains_key(&5));
    }

    #[test]
    fn cancelled_transfer_does_not_resurrect() {
        let mut r = make_receiver();

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 5,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"data"),
        }));
        r.process_message(Message::TransferCancel { transfer_number: 5 });

        // A repeated segment (Section 6 repetition) arrives after the Cancel;
        // Section 4.2: it MUST NOT re-create the transfer.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 5,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"data"),
        }));

        assert!(!r.transfers.contains_key(&5));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 5,
                reason: DropReason::Cancelled,
            }
        )));

        // Same for a late End; and it must not deliver a bundle.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 5,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"tail"),
        }));
        assert!(!r.transfers.contains_key(&5));
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived { .. }))
        );
    }

    #[test]
    fn repeated_cancel_is_idempotent() {
        let mut r = make_receiver();

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 5,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"data"),
        }));

        let first = r.process_message(Message::TransferCancel { transfer_number: 5 });
        assert!(
            first
                .iter()
                .any(|e| matches!(e, ReceiverEvent::TransferCancelled { transfer_number: 5 }))
        );

        // A repeated Cancel is reported as a drop, not a second cancellation.
        let second = r.process_message(Message::TransferCancel { transfer_number: 5 });
        assert!(
            second
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::TransferCancelled { .. }))
        );
        assert!(second.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 5,
                reason: DropReason::Cancelled,
            }
        )));
    }

    #[test]
    fn cancel_of_unknown_transfer_ignored() {
        let mut r = make_receiver();

        // Start transfer 0 so we can observe window side effects.
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"hel"),
        }));

        // Section 8.4: Cancel for a never-seen transfer number is ignored --
        // no TransferCancelled, and no window advance (a large number here
        // would otherwise expire transfer 0).
        let events = r.process_message(Message::TransferCancel {
            transfer_number: 1000,
        });
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::TransferCancelled { .. }))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 1000,
                reason: DropReason::UnknownTransfer,
            }
        )));

        // Transfer 0 survived and still completes.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"lo"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data: b, .. } if b.as_ref() == b"hello"
        )));
    }

    #[test]
    fn cancelled_set_pruned_by_window_advance() {
        let mut r = Receiver::new(ws(4), MaxBundleSize::default());

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"x"),
        }));
        r.process_message(Message::TransferCancel { transfer_number: 0 });
        assert!(r.abandoned.contains_key(&0));

        // Advance the window until 0 falls out of it.
        for t in 1..=4u32 {
            r.process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: t,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"x"),
            }));
        }
        assert!(r.abandoned.is_empty());

        // A really late segment for 0 is now dropped as out-of-window.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"x"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::OutsideWindow,
            }
        )));
        assert!(!r.transfers.contains_key(&0));
    }

    #[test]
    fn outside_window_drop_reported() {
        let mut r = Receiver::new(ws(4), MaxBundleSize::default());

        for t in 0..8u32 {
            r.process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: t,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"x"),
            }));
        }

        // greatest = 7, window = 4: transfer 0 is well outside.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"y"),
        }));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::OutsideWindow,
            }
        ));
    }

    #[test]
    fn window_expiry() {
        let mut r = Receiver::new(ws(4), MaxBundleSize::default());

        // Create transfers 0, 1, 2, 3
        for t in 0..4u32 {
            r.process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: t,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"x"),
            }));
        }
        assert_eq!(r.transfers.len(), 4);

        // New transfer 4 should expire transfer 0
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 4,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"y"),
        }));

        let expired: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ReceiverEvent::TransferExpired { .. }))
            .collect();
        assert!(!expired.is_empty());
        assert!(
            expired
                .iter()
                .any(|e| matches!(e, ReceiverEvent::TransferExpired { transfer_number: 0 }))
        );
    }

    #[test]
    fn fec_core_mixing_dropped() {
        let mut r = make_receiver();

        // Start a core transfer
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"hel"),
        }));

        // An FEC message on the same transfer number is dropped as a
        // per-message disposition, not an error.
        let events = r.process_message(Message::PreAgreedFecSource(
            crate::fec::PreAgreedFecSourceMessage {
                transfer_number: 0,
                fec_instance_id: 1,
                hints: vec![],
                payload: Bytes::from_static(b"fec"),
            },
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::FecCoreMixing,
            }
        )));

        // The core transfer is untouched and still completes.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"lo"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data: b, .. } if b.as_ref() == b"hello"
        )));
    }

    /// Encode a two-segment transfer of b"hello" into `pdu`.
    fn put_completing_transfer(pdu: &mut BytesMut) {
        codec::encode_message(
            &Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"hel"),
            }),
            pdu,
        )
        .unwrap();
        codec::encode_message(
            &Message::TransferEnd(TransferEndMessage {
                transfer_number: 0,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"lo"),
            }),
            pdu,
        )
        .unwrap();
    }

    #[test]
    fn malformed_message_mid_pdu_keeps_prior_events_and_continues() {
        let mut r = make_receiver();

        // One PDU: a transfer that completes, then a known-type message with
        // a malformed interior, then a well-formed Bundle.
        let mut pdu = BytesMut::new();
        put_completing_transfer(&mut pdu);
        // Bundle message, H flag set, content = malformed hint chain (a hint
        // header promising a 255-byte value with no bytes behind it).
        pdu.put_u8(0x02);
        pdu.put_u8(0x80);
        pdu.put_u16(2);
        pdu.put_slice(b"\x1F\xFF");
        codec::encode_message(
            &Message::Bundle {
                hints: vec![],
                data: Bytes::from_static(b"ok"),
            },
            &mut pdu,
        )
        .unwrap();

        // The completed transfer's bundle survives the later fault, the bad
        // message is reported and skipped, and the trailing Bundle is still
        // processed via the next header-length boundary.
        let events = r.receive_pdu(pdu.freeze());
        assert!(matches!(
            &events[..],
            [
                ReceiverEvent::BundleReceived { data: hello, .. },
                ReceiverEvent::MalformedMessage {
                    error: codec::Error::InsufficientData { .. },
                },
                ReceiverEvent::BundleReceived { data: ok, .. },
            ] if hello.as_ref() == b"hello" && ok.as_ref() == b"ok"
        ));
    }

    #[test]
    fn malformed_pdu_keeps_prior_events() {
        let mut r = make_receiver();

        // A completing transfer followed by a truncated header: the trailing
        // bytes are undecodable (no message boundary), but the bundle
        // completed earlier in the PDU must still be delivered alongside the
        // MalformedPdu report.
        let mut pdu = BytesMut::new();
        put_completing_transfer(&mut pdu);
        pdu.put_slice(&[0x02, 0x30]); // 2 bytes: too short for a 4-byte header

        let events = r.receive_pdu(pdu.freeze());
        assert!(matches!(
            &events[..],
            [
                ReceiverEvent::BundleReceived { data: hello, .. },
                ReceiverEvent::MalformedPdu {
                    error: codec::Error::InsufficientData { .. },
                },
            ] if hello.as_ref() == b"hello"
        ));
    }

    #[test]
    fn oversized_bundle_message_rejected_with_event() {
        let mut r = Receiver::new(WindowSize::default(), MaxBundleSize::try_from(5).unwrap());

        // Bundle message exceeding the limit: observable, not delivered.
        let events = r.process_message(Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"too long bundle"),
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            ReceiverEvent::BundleRejected { len: 15 }
        ));

        // Exactly at the limit is accepted.
        let events = r.process_message(Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"12345"),
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ReceiverEvent::BundleReceived { data, .. } if data.as_ref() == b"12345"
        ));
    }

    #[test]
    fn oversized_transfer_rejected_during_accumulation() {
        let mut r = Receiver::new(WindowSize::default(), MaxBundleSize::try_from(5).unwrap());

        // 3 bytes: under the limit, transfer stays alive.
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"abc"),
        }));
        assert!(r.transfers.contains_key(&0));

        // 6 accumulated bytes: rejected immediately, no waiting for the End.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"def"),
        }));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReceiverEvent::TransferRejected { transfer_number: 0 }))
        );
        assert!(!r.transfers.contains_key(&0));

        // A further segment must not re-create the rejected transfer.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 2,
            hints: vec![],
            data: Bytes::from_static(b"x"),
        }));
        assert!(!r.transfers.contains_key(&0));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 0,
                reason: DropReason::TooLarge,
            }
        )));
    }

    #[test]
    fn bundle_length_hint_rejects_early() {
        let mut r = Receiver::new(WindowSize::default(), MaxBundleSize::try_from(5).unwrap());

        // The first segment is tiny, but the hint promises 100 bytes:
        // reject on the spot rather than accumulating toward the limit.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![HintItem::BundleLength(100)],
            data: Bytes::from_static(b"a"),
        }));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReceiverEvent::TransferRejected { transfer_number: 0 }))
        );
        assert!(!r.transfers.contains_key(&0));
    }

    #[test]
    fn transfer_exactly_max_size_accepted() {
        let mut r = Receiver::new(WindowSize::default(), MaxBundleSize::try_from(6).unwrap());

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![HintItem::BundleLength(6)],
            data: Bytes::from_static(b"abc"),
        }));
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"def"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data: b, .. } if b.as_ref() == b"abcdef"
        )));
    }

    #[test]
    fn max_bundle_size_zero_rejected() {
        assert!(matches!(
            MaxBundleSize::try_from(0),
            Err(Error::InvalidMaxBundleSize(0))
        ));
        assert_eq!(MaxBundleSize::try_from(1).unwrap().get(), 1);
        assert_eq!(MaxBundleSize::default().get(), 0x4000_0000);
    }

    #[test]
    fn empty_segment_dropped_without_side_effects() {
        let mut r = make_receiver();

        // Start transfer 0 so window side effects would be observable.
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"hel"),
        }));

        // An empty segment with a huge transfer number is dropped before any
        // state is touched: no transfer created, and no window advance
        // (which would otherwise expire transfer 0).
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 1000,
            segment_index: 0,
            hints: vec![],
            data: Bytes::new(),
        }));
        assert!(matches!(
            &events[..],
            [ReceiverEvent::MessageDropped {
                transfer_number: 1000,
                reason: DropReason::EmptySegment,
            }]
        ));
        assert!(!r.transfers.contains_key(&1000));

        // An empty segment on an existing transfer stores nothing either:
        // this is what keeps the segment map bounded by the byte cap.
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 5,
            hints: vec![],
            data: Bytes::new(),
        }));
        assert_eq!(r.transfers[&0].segments.len(), 1);

        // Transfer 0 is unaffected and still completes.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"lo"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data: b, .. } if b.as_ref() == b"hello"
        )));
    }

    #[test]
    fn empty_end_fixes_final_index_but_stores_nothing() {
        let mut r = make_receiver();

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"ab"),
        }));

        // Section 8.3 SHOULD NOT: an empty End records the final index but
        // stores no segment data, so the transfer is not complete.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::new(),
        }));
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived { .. }))
        );
        assert_eq!(r.transfers[&0].segments.len(), 1);

        // A repeated final segment carrying the data (Section 6 repetition)
        // completes the transfer under the index the End fixed.
        let events = r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"cd"),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data: b, .. } if b.as_ref() == b"abcd"
        )));
    }

    #[test]
    fn single_segment_transfer_shares_the_segment_bytes() {
        let mut r = make_receiver();
        let payload = Bytes::from_static(b"solo bundle");
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: payload.clone(),
        }));
        // Delivered without copying: the event's Bytes views the same
        // allocation as the received segment (a refcount bump, not a
        // rebuild).
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data, .. }
                if data.as_ref() == b"solo bundle" && data.as_ptr() == payload.as_ptr()
        )));
    }

    #[test]
    fn bundle_received_surfaces_transfer_hints() {
        let mut r = make_receiver();
        let correlator_v1 = HintItem::Unknown {
            hint_type: 0x41,
            value: Bytes::from_static(b"\x07"),
        };
        let correlator_v2 = HintItem::Unknown {
            hint_type: 0x41,
            value: Bytes::from_static(b"\x09"),
        };

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![HintItem::BundleLength(5), correlator_v1],
            data: Bytes::from_static(b"hel"),
        }));
        // A later message repeating the hint type supersedes the value.
        let events = r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![correlator_v2.clone()],
            data: Bytes::from_static(b"lo"),
        }));

        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived { data, hints }
                if data.as_ref() == b"hello"
                    && *hints == vec![HintItem::BundleLength(5), correlator_v2.clone()]
        )));
    }

    #[test]
    fn bundle_message_hints_deduped_latest_wins() {
        // The Bundle message path honours the same BundleReceived contract as
        // the transfer path: one item per hint type, latest wins, ordered by
        // type.
        let mut r = make_receiver();
        let stale = HintItem::Unknown {
            hint_type: 0x41,
            value: Bytes::from_static(b"old"),
        };
        let fresh = HintItem::Unknown {
            hint_type: 0x41,
            value: Bytes::from_static(b"new"),
        };
        let events = r.process_message(Message::Bundle {
            // Deliberately out of type order, with the repeat last.
            hints: vec![stale, HintItem::BundleLength(2), fresh.clone()],
            data: Bytes::from_static(b"hi"),
        });
        assert!(matches!(
            &events[..],
            [ReceiverEvent::BundleReceived { hints, .. }]
                if *hints == vec![HintItem::BundleLength(2), fresh.clone()]
        ));
    }

    #[test]
    fn bundle_message_hints_surfaced() {
        let mut r = make_receiver();
        let hint = HintItem::Unknown {
            hint_type: 0x41,
            value: Bytes::from_static(b"z"),
        };
        let events = r.process_message(Message::Bundle {
            hints: vec![hint.clone()],
            data: Bytes::from_static(b"hi"),
        });
        assert!(matches!(
            &events[..],
            [ReceiverEvent::BundleReceived { data, hints }]
                if data.as_ref() == b"hi" && *hints == vec![hint.clone()]
        ));
    }

    #[test]
    fn sender_receiver_round_trip() {
        use crate::sender::{PduSize, SendOpts, SendQueueDepth, Sender};

        let pdu_size = 64;
        let mut sender = Sender::new(
            PduSize::try_from(pdu_size).unwrap(),
            WindowSize::default(),
            SendQueueDepth::default(),
            0,
        );
        let mut receiver = make_receiver();

        let original = Bytes::from(vec![0x42; 200]);
        sender
            .enqueue(original.clone(), SendOpts::default())
            .unwrap();

        let mut all_events = Vec::new();
        while sender.has_pending() {
            let pdu = sender.next_pdu().unwrap();
            let events = receiver.receive_pdu(pdu.freeze());
            all_events.extend(events);
        }

        let received: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                ReceiverEvent::BundleReceived { data, .. } => Some(data.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].as_ref(), original.as_ref());
    }

    #[test]
    fn sender_receiver_round_trip_small() {
        use crate::sender::{PduSize, SendOpts, SendQueueDepth, Sender};

        let pdu_size = 256;
        let mut sender = Sender::new(
            PduSize::try_from(pdu_size).unwrap(),
            WindowSize::default(),
            SendQueueDepth::default(),
            0,
        );
        let mut receiver = make_receiver();

        let original = Bytes::from_static(b"tiny");
        sender
            .enqueue(original.clone(), SendOpts::default())
            .unwrap();

        let pdu = sender.next_pdu().unwrap();
        let events = receiver.receive_pdu(pdu.freeze());

        let received: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ReceiverEvent::BundleReceived { data, .. } => Some(data.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].as_ref(), b"tiny");
    }
}
