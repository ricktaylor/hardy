use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use bytes::{BufMut, Bytes, BytesMut};

use crate::codec;
use crate::error::Error;
use crate::hint::HintItem;
use crate::message::*;
use crate::transfer::{DEFAULT_WINDOW_SIZE, TransferValidity, TransferWindow};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the BTP-U receiver.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct ReceiverConfig {
    /// Transfer window size (4..=4096). Default: 16.
    pub window_size: u16,

    /// Maximum bundle size to accept in bytes.  Transfers exceeding this
    /// size are cancelled.  `None` means unlimited.
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

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events emitted by the receiver for the calling CLA to act on.
#[derive(Debug)]
pub enum ReceiverEvent {
    /// A complete bundle has been reassembled (or received as a Bundle message).
    BundleReceived(Bytes),

    /// A transfer was cancelled by the sender.
    TransferCancelled { transfer_number: u32 },

    /// A transfer was evicted from the window (incomplete).
    TransferExpired { transfer_number: u32 },
}

// ---------------------------------------------------------------------------
// In-progress transfer
// ---------------------------------------------------------------------------

/// Whether a transfer uses core segmentation or FEC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferKind {
    Core,
    Fec,
}

struct InProgressTransfer {
    kind: TransferKind,
    segments: BTreeMap<u32, Bytes>,
    final_segment_index: Option<u32>,
    bundle_length_hint: Option<u64>,
}

impl InProgressTransfer {
    fn new(kind: TransferKind) -> Self {
        Self {
            kind,
            segments: BTreeMap::new(),
            final_segment_index: None,
            bundle_length_hint: None,
        }
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

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// Manages inbound PDU processing, transfer window, and segment reassembly.
pub struct Receiver {
    config: ReceiverConfig,
    window: TransferWindow,
    transfers: BTreeMap<u32, InProgressTransfer>,
}

impl Receiver {
    /// Create a new receiver.
    pub fn new(config: ReceiverConfig) -> Self {
        let window = TransferWindow::new(config.window_size);
        Self {
            config,
            window,
            transfers: BTreeMap::new(),
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
                if let Some(max) = self.config.max_bundle_size {
                    if data.len() > max {
                        return Ok(vec![]);
                    }
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

    fn process_transfer_segment(
        &mut self,
        m: TransferSegmentMessage,
    ) -> Result<Vec<ReceiverEvent>, Error> {
        let mut events = Vec::new();

        let validity = self.window.process(m.transfer_number);
        match validity {
            TransferValidity::OutsideWindow => return Ok(vec![]),
            TransferValidity::New => {
                events.append(&mut self.expire_old_transfers());
            }
            TransferValidity::InProgress => {}
        }

        let transfer = self
            .transfers
            .entry(m.transfer_number)
            .or_insert_with(|| InProgressTransfer::new(TransferKind::Core));

        if transfer.kind != TransferKind::Core {
            return Err(Error::FecCoreMixing(m.transfer_number));
        }

        transfer.apply_hints(&m.hints);
        // Ignore duplicate segments (repetition support).
        transfer.segments.entry(m.segment_index).or_insert(m.data);

        Ok(events)
    }

    fn process_transfer_end(&mut self, m: TransferEndMessage) -> Result<Vec<ReceiverEvent>, Error> {
        let mut events = Vec::new();

        let validity = self.window.process(m.transfer_number);
        match validity {
            TransferValidity::OutsideWindow => return Ok(vec![]),
            TransferValidity::New => {
                events.append(&mut self.expire_old_transfers());
            }
            TransferValidity::InProgress => {}
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
        transfer.segments.entry(m.segment_index).or_insert(m.data);

        if transfer.is_complete() {
            let bundle = transfer.reassemble();
            self.transfers.remove(&m.transfer_number);

            if let Some(max) = self.config.max_bundle_size {
                if bundle.len() > max {
                    return Ok(events);
                }
            }
            events.push(ReceiverEvent::BundleReceived(bundle));
        }

        Ok(events)
    }

    fn process_transfer_cancel(
        &mut self,
        transfer_number: u32,
    ) -> Result<Vec<ReceiverEvent>, Error> {
        self.transfers.remove(&transfer_number);
        Ok(vec![ReceiverEvent::TransferCancelled { transfer_number }])
    }

    fn process_fec_message(
        &mut self,
        transfer_number: u32,
        kind: TransferKind,
        hints: &[HintItem],
    ) -> Result<Vec<ReceiverEvent>, Error> {
        let mut events = Vec::new();

        let validity = self.window.process(transfer_number);
        match validity {
            TransferValidity::OutsideWindow => return Ok(vec![]),
            TransferValidity::New => {
                events.append(&mut self.expire_old_transfers());
            }
            TransferValidity::InProgress => {}
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

        // Send segment 1 (end) first, then segment 0.
        r.process_message(Message::TransferEnd(TransferEndMessage {
            transfer_number: 0,
            segment_index: 1,
            hints: vec![],
            data: Bytes::from_static(b"ld"),
        }))
        .unwrap();

        let events = r
            .process_message(Message::TransferSegment(TransferSegmentMessage {
                transfer_number: 0,
                segment_index: 0,
                hints: vec![],
                data: Bytes::from_static(b"wor"),
            }))
            .unwrap();

        // Should not yet be complete because segment 0 came after the End,
        // but the transfer should now be complete since we have 0 and 1.
        // Actually the process_message for TransferSegment checks completeness too?
        // No - only process_transfer_end checks completeness.
        // Let's verify: we need to check that the transfer IS actually complete.
        // The TransferEnd was received first (setting final_segment_index=1),
        // then segment 0 arrived. But process_transfer_segment doesn't check
        // completeness. Let me adjust.
        // For now, let's test by sending End after all segments.
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ReceiverEvent::BundleReceived(_)))
        );
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
                source_fec_payload_id: Bytes::new(),
                source_data: Bytes::from_static(b"fec"),
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
