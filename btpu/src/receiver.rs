use crate::codec;
use crate::hint::HintItem;
use crate::message::*;
use crate::transfer::{DEFAULT_WINDOW_SIZE, TransferValidity, TransferWindow};
use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry;
use alloc::vec;
use alloc::vec::Vec;
use bytes::{BufMut, Bytes, BytesMut};

/// Errors from inbound PDU processing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The PDU could not be decoded.
    #[error(transparent)]
    Codec(#[from] crate::codec::Error),

    /// FEC and core transfer messages were mixed in the same transfer.
    #[error("Cannot mix FEC and core transfer messages in transfer {0}")]
    FecCoreMixing(u32),
}

/// Configuration for the BTP-U receiver.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct ReceiverConfig {
    /// Transfer window size (4..=4095). Default: 16.
    pub window_size: u16,

    /// Maximum bundle size to accept in bytes.  Transfers exceeding this
    /// size are rejected ([`ReceiverEvent::TransferRejected`]).
    /// Default: `None` (unlimited).
    pub max_bundle_size: Option<usize>,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            window_size: DEFAULT_WINDOW_SIZE,
            max_bundle_size: None,
        }
    }
}

/// Why an otherwise well-formed message was not applied to a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropReason {
    /// The transfer was previously cancelled by the sender (Section 4.2).
    Cancelled,
    /// The transfer number is outside the current receive window (Section 5).
    OutsideWindow,
    /// A Cancel referenced a transfer that is not in progress (Section 8.4).
    UnknownTransfer,
    /// The transfer was rejected for exceeding the configured
    /// [`ReceiverConfig::max_bundle_size`].
    TooLarge,
}

/// Events emitted by the receiver for the calling CLA to act on.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReceiverEvent {
    /// A complete bundle has been reassembled (or received as a Bundle message).
    BundleReceived(Bytes),

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
    /// segment data, or the sender's Bundle Length hint, exceeds
    /// [`ReceiverConfig::max_bundle_size`].
    TransferRejected { transfer_number: u32 },
}

/// Whether a transfer uses core segmentation or FEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferKind {
    Core,
    Fec,
}

/// Outcome of the per-message admission check ([`Receiver::gate_admission`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// The message may be applied to its transfer.
    Proceed,
    /// The message must be dropped; a report event has been pushed.
    Halt,
}

struct InProgressTransfer {
    kind: TransferKind,
    segments: BTreeMap<u32, Bytes>,
    final_segment_index: Option<u32>,
    bundle_length_hint: Option<u64>,
    /// Total bytes across accepted (non-duplicate) segments.
    received_bytes: usize,
}

impl InProgressTransfer {
    fn new(kind: TransferKind) -> Self {
        Self {
            kind,
            segments: BTreeMap::new(),
            final_segment_index: None,
            bundle_length_hint: None,
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
        self.received_bytes > max || self.bundle_length_hint.is_some_and(|h| h > max as u64)
    }

    /// Record hints from a message.
    fn apply_hints(&mut self, hints: &[HintItem]) {
        for h in hints {
            if let HintItem::BundleLength(len) = h {
                self.bundle_length_hint = Some(*len);
            }
        }
    }

    /// Check whether all segments 0..=N have been received.
    fn is_complete(&self) -> bool {
        if let Some(n) = self.final_segment_index {
            let expected = n + 1;
            self.segments.len() as u32 == expected
                && *self.segments.keys().last().unwrap() == n
                && *self.segments.keys().next().unwrap() == 0
        } else {
            false
        }
    }

    /// Concatenate segments in order and return the reassembled bundle.
    fn reassemble(&self) -> Bytes {
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
    config: ReceiverConfig,
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
    pub fn new(config: ReceiverConfig) -> Self {
        let window = TransferWindow::new(config.window_size);
        Self {
            config,
            window,
            transfers: BTreeMap::new(),
            abandoned: BTreeMap::new(),
        }
    }

    /// Process a received convergence layer PDU.  Returns zero or more events.
    ///
    /// Taking `pdu` by value (rather than `&[u8]`) lets the codec extract
    /// message data as zero-copy [`Bytes`] views into the original buffer.
    pub fn receive_pdu(&mut self, pdu: Bytes) -> Result<Vec<ReceiverEvent>, Error> {
        let messages = codec::decode_pdu(pdu)?;
        let mut events = Vec::new();
        for msg in messages {
            let mut msg_events = self.process_message(msg)?;
            events.append(&mut msg_events);
        }
        Ok(events)
    }

    /// Process a single decoded message.
    pub fn process_message(&mut self, message: Message) -> Result<Vec<ReceiverEvent>, Error> {
        match message {
            Message::IndefinitePadding | Message::DefinitePadding(_) => Ok(vec![]),

            Message::Bundle { data, .. } => {
                if let Some(max) = self.config.max_bundle_size
                    && data.len() > max
                {
                    return Ok(vec![]);
                }
                Ok(vec![ReceiverEvent::BundleReceived(data)])
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

            Message::Unknown { .. } => Ok(vec![]),
        }
    }

    // -- transfer processing ------------------------------------------------

    /// Admission check: is `transfer_number` eligible for processing at all?
    /// It must be inside the receive window and not previously abandoned
    /// (cancelled or rejected).
    ///
    /// Any events produced (window-advance expiries, drop reports) are pushed
    /// onto `events`.  Returns [`Gate::Halt`] when the message must not be
    /// applied.  Both drop cases are expected traffic, not faults, so they
    /// surface as [`ReceiverEvent::MessageDropped`] rather than [`Error`]s.
    fn gate_admission(&mut self, transfer_number: u32, events: &mut Vec<ReceiverEvent>) -> Gate {
        match self.window.process(transfer_number) {
            TransferValidity::OutsideWindow => {
                events.push(ReceiverEvent::MessageDropped {
                    transfer_number,
                    reason: DropReason::OutsideWindow,
                });
                return Gate::Halt;
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
            return Gate::Halt;
        }

        Gate::Proceed
    }

    /// Enforce [`ReceiverConfig::max_bundle_size`] on an in-progress transfer.
    ///
    /// Runs after each segment insert: rejects as soon as the accumulated
    /// bytes exceed the limit, or earlier still if the sender's Bundle Length
    /// hint already promises an oversized bundle.  On rejection the transfer
    /// is dropped, recorded in `abandoned` (so repeated segments cannot
    /// re-create it), and reported via [`ReceiverEvent::TransferRejected`].
    fn gate_oversize(&mut self, transfer_number: u32, events: &mut Vec<ReceiverEvent>) -> Gate {
        let Some(max) = self.config.max_bundle_size else {
            return Gate::Proceed;
        };
        let oversized = self
            .transfers
            .get(&transfer_number)
            .is_some_and(|t| t.exceeds(max));
        if !oversized {
            return Gate::Proceed;
        }

        self.transfers.remove(&transfer_number);
        self.abandoned.insert(transfer_number, DropReason::TooLarge);
        events.push(ReceiverEvent::TransferRejected { transfer_number });
        Gate::Halt
    }

    fn process_transfer_segment(
        &mut self,
        m: TransferSegmentMessage,
    ) -> Result<Vec<ReceiverEvent>, Error> {
        let mut events = Vec::new();
        if self.gate_admission(m.transfer_number, &mut events) == Gate::Halt {
            return Ok(events);
        }

        let transfer = self
            .transfers
            .entry(m.transfer_number)
            .or_insert_with(|| InProgressTransfer::new(TransferKind::Core));

        if transfer.kind != TransferKind::Core {
            return Err(Error::FecCoreMixing(m.transfer_number));
        }

        transfer.apply_hints(&m.hints);
        transfer.insert_segment(m.segment_index, m.data);

        if self.gate_oversize(m.transfer_number, &mut events) == Gate::Halt {
            return Ok(events);
        }

        // A late segment may fill the final gap of a transfer whose End was
        // already received; check completeness on every insert, not just End.
        self.complete_if_ready(m.transfer_number, &mut events);

        Ok(events)
    }

    fn process_transfer_end(&mut self, m: TransferEndMessage) -> Result<Vec<ReceiverEvent>, Error> {
        let mut events = Vec::new();
        if self.gate_admission(m.transfer_number, &mut events) == Gate::Halt {
            return Ok(events);
        }

        let transfer = self
            .transfers
            .entry(m.transfer_number)
            .or_insert_with(|| InProgressTransfer::new(TransferKind::Core));

        if transfer.kind != TransferKind::Core {
            return Err(Error::FecCoreMixing(m.transfer_number));
        }

        transfer.apply_hints(&m.hints);
        transfer.final_segment_index = Some(m.segment_index);
        transfer.insert_segment(m.segment_index, m.data);

        if self.gate_oversize(m.transfer_number, &mut events) == Gate::Halt {
            return Ok(events);
        }

        self.complete_if_ready(m.transfer_number, &mut events);

        Ok(events)
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
        let transfer = self.transfers.remove(&transfer_number).unwrap();
        events.push(ReceiverEvent::BundleReceived(transfer.reassemble()));
    }

    fn process_transfer_cancel(
        &mut self,
        transfer_number: u32,
    ) -> Result<Vec<ReceiverEvent>, Error> {
        // Section 8.4: a Cancel for an unknown transfer MUST be ignored.
        // "Ignored" includes side effects, so no gate_admission here: a Cancel
        // never advances the window.  In-progress transfers are in-window by
        // construction, so a valid Cancel has no window state to update.
        if self.transfers.remove(&transfer_number).is_some() {
            self.abandoned
                .insert(transfer_number, DropReason::Cancelled);
            return Ok(vec![ReceiverEvent::TransferCancelled { transfer_number }]);
        }

        // Distinguish a repeated Cancel of an already-abandoned transfer
        // (idempotent, reported with the original abandon reason) from a
        // never-seen number.
        let reason = self
            .abandoned
            .get(&transfer_number)
            .copied()
            .unwrap_or(DropReason::UnknownTransfer);
        Ok(vec![ReceiverEvent::MessageDropped {
            transfer_number,
            reason,
        }])
    }

    fn process_fec_message(
        &mut self,
        transfer_number: u32,
        kind: TransferKind,
        hints: &[HintItem],
    ) -> Result<Vec<ReceiverEvent>, Error> {
        let mut events = Vec::new();
        if self.gate_admission(transfer_number, &mut events) == Gate::Halt {
            return Ok(events);
        }

        let transfer = self
            .transfers
            .entry(transfer_number)
            .or_insert_with(|| InProgressTransfer::new(kind));

        if transfer.kind != kind {
            return Err(Error::FecCoreMixing(transfer_number));
        }

        transfer.apply_hints(hints);
        // FEC reassembly requires a registered FecScheme; without one we just
        // track the transfer to maintain correct window state.

        Ok(events)
    }

    // -- window expiry ------------------------------------------------------

    fn expire_old_transfers(&mut self) -> Vec<ReceiverEvent> {
        let active: Vec<u32> = self.transfers.keys().copied().collect();
        let expired = self.window.expired_transfers(active.iter());
        let mut events = Vec::new();
        for t in expired {
            self.transfers.remove(&t);
            events.push(ReceiverEvent::TransferExpired { transfer_number: t });
        }

        // Prune the abandoned map the same way; this is what keeps it bounded
        // by the window size.  No events: these were already reported as
        // TransferCancelled / TransferRejected when they were abandoned.
        for t in self.window.expired_transfers(self.abandoned.keys()) {
            self.abandoned.remove(&t);
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_receiver() -> Receiver {
        Receiver::new(ReceiverConfig {
            window_size: 16,
            max_bundle_size: None,
        })
    }

    #[test]
    fn bundle_message_immediate() {
        let mut r = make_receiver();
        let msg = Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"hello"),
        };
        let events = r.process_message(msg).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ReceiverEvent::BundleReceived(data) => assert_eq!(data.as_ref(), b"hello"),
            other => panic!("Expected BundleReceived, got {other:?}"),
        }
    }

    #[test]
    fn two_segment_transfer() {
        let mut r = make_receiver();

        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"hel"),
            }))
            .unwrap();
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived(_)))
        );

        let events = r
            .process_message(Message::TransferEnd(TransferEndMessage {
                transfer_number: 0,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"lo"),
            }))
            .unwrap();

        let received: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ReceiverEvent::BundleReceived(_)))
            .collect();
        assert_eq!(received.len(), 1);
        match &received[0] {
            ReceiverEvent::BundleReceived(data) => assert_eq!(data.as_ref(), b"hello"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn out_of_order_segments() {
        let mut r = make_receiver();

        // Send segment 1 (the End) first, then the late segment 0.  The End
        // fixes final_segment_index=1 but the transfer is not yet complete.
        let events = r
            .process_message(Message::TransferEnd(TransferEndMessage {
                transfer_number: 0,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"ld"),
            }))
            .unwrap();
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived(_)))
        );

        // The late segment 0 fills the final gap; completion must fire here,
        // on the segment insert, even though the End arrived earlier.
        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"wor"),
            }))
            .unwrap();

        let received: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ReceiverEvent::BundleReceived(_)))
            .collect();
        assert_eq!(received.len(), 1);
        match &received[0] {
            ReceiverEvent::BundleReceived(data) => assert_eq!(data.as_ref(), b"world"),
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
        }))
        .unwrap();

        // Segment 0
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"wor"),
        }))
        .unwrap();

        // Transfer End with segment 2
        let events = r
            .process_message(Message::TransferEnd(TransferEndMessage {
                transfer_number: 0,
                segment_index: 2,
                hints: vec![],
                data: Bytes::from_static(b"!"),
            }))
            .unwrap();

        let received: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ReceiverEvent::BundleReceived(_)))
            .collect();
        assert_eq!(received.len(), 1);
        match &received[0] {
            ReceiverEvent::BundleReceived(data) => assert_eq!(data.as_ref(), b"world!"),
            _ => unreachable!(),
        }
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
        }))
        .unwrap();

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"SHOULD BE IGNORED"),
        }))
        .unwrap();

        let events = r
            .process_message(Message::TransferEnd(TransferEndMessage {
                transfer_number: 0,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"def"),
            }))
            .unwrap();

        match &events.last().unwrap() {
            ReceiverEvent::BundleReceived(data) => assert_eq!(data.as_ref(), b"abcdef"),
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
        }))
        .unwrap();

        let events = r
            .process_message(Message::TransferCancel { transfer_number: 5 })
            .unwrap();

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
        }))
        .unwrap();
        r.process_message(Message::TransferCancel { transfer_number: 5 })
            .unwrap();

        // A repeated segment (Section 6 repetition) arrives after the Cancel;
        // Section 4.2: it MUST NOT re-create the transfer.
        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 5,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"data"),
            }))
            .unwrap();

        assert!(!r.transfers.contains_key(&5));
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::MessageDropped {
                transfer_number: 5,
                reason: DropReason::Cancelled,
            }
        )));

        // Same for a late End; and it must not deliver a bundle.
        let events = r
            .process_message(Message::TransferEnd(TransferEndMessage {
                transfer_number: 5,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"tail"),
            }))
            .unwrap();
        assert!(!r.transfers.contains_key(&5));
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived(_)))
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
        }))
        .unwrap();

        let first = r
            .process_message(Message::TransferCancel { transfer_number: 5 })
            .unwrap();
        assert!(
            first
                .iter()
                .any(|e| matches!(e, ReceiverEvent::TransferCancelled { transfer_number: 5 }))
        );

        // A repeated Cancel is reported as a drop, not a second cancellation.
        let second = r
            .process_message(Message::TransferCancel { transfer_number: 5 })
            .unwrap();
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
        }))
        .unwrap();

        // Section 8.4: Cancel for a never-seen transfer number is ignored --
        // no TransferCancelled, and no window advance (a large number here
        // would otherwise expire transfer 0).
        let events = r
            .process_message(Message::TransferCancel {
                transfer_number: 1000,
            })
            .unwrap();
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
        let events = r
            .process_message(Message::TransferEnd(TransferEndMessage {
                transfer_number: 0,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"lo"),
            }))
            .unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived(b) if b.as_ref() == b"hello"
        )));
    }

    #[test]
    fn cancelled_set_pruned_by_window_advance() {
        let mut r = Receiver::new(ReceiverConfig {
            window_size: 4,
            max_bundle_size: None,
        });

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"x"),
        }))
        .unwrap();
        r.process_message(Message::TransferCancel { transfer_number: 0 })
            .unwrap();
        assert!(r.abandoned.contains_key(&0));

        // Advance the window until 0 falls out of it.
        for t in 1..=4u32 {
            r.process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: t,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"x"),
            }))
            .unwrap();
        }
        assert!(r.abandoned.is_empty());

        // A really late segment for 0 is now dropped as out-of-window.
        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"x"),
            }))
            .unwrap();
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
        let mut r = Receiver::new(ReceiverConfig {
            window_size: 4,
            max_bundle_size: None,
        });

        for t in 0..8u32 {
            r.process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: t,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"x"),
            }))
            .unwrap();
        }

        // greatest = 7, window = 4: transfer 0 is well outside.
        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"y"),
            }))
            .unwrap();
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
        let mut r = Receiver::new(ReceiverConfig {
            window_size: 4,
            max_bundle_size: None,
        });

        // Create transfers 0, 1, 2, 3
        for t in 0..4u32 {
            r.process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: t,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"x"),
            }))
            .unwrap();
        }
        assert_eq!(r.transfers.len(), 4);

        // New transfer 4 should expire transfer 0
        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 4,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"y"),
            }))
            .unwrap();

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
    fn fec_core_mixing_rejected() {
        let mut r = make_receiver();

        // Start a core transfer
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"core"),
        }))
        .unwrap();

        // Try to add an FEC message with the same transfer number
        let result = r.process_message(Message::PreAgreedFecSource(
            crate::fec::PreAgreedFecSourceMessage {
                transfer_number: 0,
                fec_instance_id: 1,
                hints: vec![],
                payload: Bytes::from_static(b"fec"),
            },
        ));
        assert!(result.is_err());
    }

    #[test]
    fn max_bundle_size_enforced() {
        let mut r = Receiver::new(ReceiverConfig {
            window_size: 16,
            max_bundle_size: Some(5),
        });

        // Bundle message exceeding limit
        let events = r
            .process_message(Message::Bundle {
                hints: vec![],
                data: Bytes::from_static(b"too long bundle"),
            })
            .unwrap();
        assert!(events.is_empty());

        // Bundle within limit
        let events = r
            .process_message(Message::Bundle {
                hints: vec![],
                data: Bytes::from_static(b"ok"),
            })
            .unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn oversized_transfer_rejected_during_accumulation() {
        let mut r = Receiver::new(ReceiverConfig {
            window_size: 16,
            max_bundle_size: Some(5),
        });

        // 3 bytes: under the limit, transfer stays alive.
        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"abc"),
        }))
        .unwrap();
        assert!(r.transfers.contains_key(&0));

        // 6 accumulated bytes: rejected immediately, no waiting for the End.
        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"def"),
            }))
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReceiverEvent::TransferRejected { transfer_number: 0 }))
        );
        assert!(!r.transfers.contains_key(&0));

        // A further segment must not re-create the rejected transfer.
        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 2,
                hints: vec![],
                data: Bytes::from_static(b"x"),
            }))
            .unwrap();
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
        let mut r = Receiver::new(ReceiverConfig {
            window_size: 16,
            max_bundle_size: Some(5),
        });

        // The first segment is tiny, but the hint promises 100 bytes:
        // reject on the spot rather than accumulating toward the limit.
        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 0,
                hints: vec![HintItem::BundleLength(100)],
                data: Bytes::from_static(b"a"),
            }))
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReceiverEvent::TransferRejected { transfer_number: 0 }))
        );
        assert!(!r.transfers.contains_key(&0));
    }

    #[test]
    fn transfer_exactly_max_size_accepted() {
        let mut r = Receiver::new(ReceiverConfig {
            window_size: 16,
            max_bundle_size: Some(6),
        });

        r.process_message(Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0,
            segment_index: 0,
            hints: vec![HintItem::BundleLength(6)],
            data: Bytes::from_static(b"abc"),
        }))
        .unwrap();
        let events = r
            .process_message(Message::TransferEnd(TransferEndMessage {
                transfer_number: 0,
                segment_index: 1,
                hints: vec![],
                data: Bytes::from_static(b"def"),
            }))
            .unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            ReceiverEvent::BundleReceived(b) if b.as_ref() == b"abcdef"
        )));
    }

    #[test]
    fn sender_receiver_round_trip() {
        use crate::sender::{Sender, SenderConfig};

        let pdu_size = 64;
        let mut sender = Sender::new(
            SenderConfig {
                pdu_size,
                window_size: 16,
            },
            0,
        );
        let mut receiver = make_receiver();

        let original = Bytes::from(vec![0x42; 200]);
        sender.enqueue(original.clone()).unwrap();

        let mut all_events = Vec::new();
        while sender.has_pending() {
            let pdu = sender.next_pdu().unwrap();
            let events = receiver.receive_pdu(pdu.freeze()).unwrap();
            all_events.extend(events);
        }

        let received: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                ReceiverEvent::BundleReceived(data) => Some(data.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].as_ref(), original.as_ref());
    }

    #[test]
    fn sender_receiver_round_trip_small() {
        use crate::sender::{Sender, SenderConfig};

        let pdu_size = 256;
        let mut sender = Sender::new(
            SenderConfig {
                pdu_size,
                window_size: 16,
            },
            0,
        );
        let mut receiver = make_receiver();

        let original = Bytes::from_static(b"tiny");
        sender.enqueue(original.clone()).unwrap();

        let pdu = sender.next_pdu().unwrap();
        let events = receiver.receive_pdu(pdu.freeze()).unwrap();

        let received: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ReceiverEvent::BundleReceived(data) => Some(data.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].as_ref(), b"tiny");
    }
}
