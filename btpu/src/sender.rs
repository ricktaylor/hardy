use alloc::collections::VecDeque;
use alloc::vec;
use bytes::{Bytes, BytesMut};

use crate::codec;
use crate::error::Error;
use crate::header::HEADER_SIZE;
use crate::hint::{self, HintItem};
use crate::message::*;
use crate::transfer::{DEFAULT_WINDOW_SIZE, TransferNumberAllocator};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the BTP-U sender.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct SenderConfig {
    /// Maximum convergence layer PDU size in bytes.
    pub pdu_size: usize,

    /// Transfer window size (4..=4096). Default: 16.
    pub window_size: u16,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            pdu_size: 1500,
            window_size: DEFAULT_WINDOW_SIZE,
        }
    }
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// Manages outbound BTP-U transfers, segmentation, and PDU packing.
///
/// The sender is convergence-layer agnostic: a CLA calls [`Sender::enqueue`] to
/// submit bundles and [`Sender::next_pdu`] to obtain packed PDU buffers
/// ready for transmission.
///
/// # Concurrency
///
/// `Sender` is designed for **single-owner** use: one task at a time mutates
/// it via `&mut self`. The `tower::Service` and `futures_core::Stream` impls
/// (under the `tower` feature) follow this contract. To share a `Sender`
/// across tasks, wrap it in `Arc<Mutex<_>>` or `tower::buffer::Buffer`; the
/// outer synchronisation serialises both the state and the waker registers
/// kept inside.
pub struct Sender {
    config: SenderConfig,
    allocator: TransferNumberAllocator,
    pending: VecDeque<Message>,
    /// Wakers stored by the `tower` Service/Stream impls. `enqueue_waker` is
    /// woken when a window slot frees; `drain_waker` is woken when a new
    /// message is pushed to `pending`. Both are single-slot — re-polling
    /// without waiting overwrites the prior registration, which matches the
    /// standard `Service`/`Stream` contract.
    #[cfg(feature = "tower")]
    enqueue_waker: Option<core::task::Waker>,
    #[cfg(feature = "tower")]
    drain_waker: Option<core::task::Waker>,
}

impl Sender {
    /// Create a new sender that will allocate `initial_transfer_number` as
    /// its first transfer number.
    ///
    /// See [`TransferNumberAllocator::new`] for the spec-recommended choice
    /// of this value, and [`Self::from_rng`] (under the `rand` feature) for
    /// the common case of seeding from an RNG.
    pub fn new(config: SenderConfig, initial_transfer_number: u32) -> Self {
        Self {
            allocator: TransferNumberAllocator::new(config.window_size, initial_transfer_number),
            pending: VecDeque::new(),
            config,
            #[cfg(feature = "tower")]
            enqueue_waker: None,
            #[cfg(feature = "tower")]
            drain_waker: None,
        }
    }

    /// Create a new sender with the initial transfer number seeded from `rng`.
    /// Convenience wrapper over [`Self::new`].
    #[cfg(feature = "rand")]
    pub fn from_rng<R: rand_core::RngCore>(config: SenderConfig, rng: &mut R) -> Self {
        Self::new(config, rng.next_u32())
    }

    /// Wake any task parked on a `Service::poll_ready` that returned
    /// `Pending` because the window was full. No-op without the `tower`
    /// feature.
    #[cfg(feature = "tower")]
    fn wake_enqueue(&mut self) {
        if let Some(w) = self.enqueue_waker.take() {
            w.wake();
        }
    }
    #[cfg(not(feature = "tower"))]
    fn wake_enqueue(&mut self) {}

    /// Wake any task parked on a `Stream::poll_next` that returned `Pending`
    /// because `pending` was empty. No-op without the `tower` feature.
    #[cfg(feature = "tower")]
    fn wake_drain(&mut self) {
        if let Some(w) = self.drain_waker.take() {
            w.wake();
        }
    }
    #[cfg(not(feature = "tower"))]
    fn wake_drain(&mut self) {}

    /// Returns the configured window size. Used by the `tower` Service
    /// impl to check capacity in `poll_ready`.
    #[cfg(feature = "tower")]
    pub(crate) fn window_size(&self) -> u16 {
        self.config.window_size
    }

    /// Returns the number of transfers currently consuming window slots.
    /// Used by the `tower` Service impl to check capacity in `poll_ready`.
    #[cfg(feature = "tower")]
    pub(crate) fn transfers_in_progress(&self) -> u32 {
        self.allocator.in_progress()
    }

    /// Register a waker to be notified when a window slot frees up.
    /// Used by the `tower` Service impl from `poll_ready`.
    #[cfg(feature = "tower")]
    pub(crate) fn register_enqueue_waker(&mut self, waker: core::task::Waker) {
        self.enqueue_waker = Some(waker);
    }

    /// Register a waker to be notified when a new PDU becomes available.
    /// Used by the `tower` Stream impl from `poll_next`.
    #[cfg(feature = "tower")]
    pub(crate) fn register_drain_waker(&mut self, waker: core::task::Waker) {
        self.drain_waker = Some(waker);
    }

    /// Queue a bundle for transmission.
    ///
    /// If the bundle fits in a single PDU (as a Bundle message), it is emitted
    /// without segmentation.  Otherwise, it is split into Transfer Segment and
    /// Transfer End messages.
    ///
    /// Returns the transfer number if segmented, or `None` if sent as a
    /// complete Bundle message.
    pub fn enqueue(&mut self, data: Bytes) -> Result<Option<u32>, Error> {
        let bundle_len = data.len();
        let max_bundle_content = self.max_single_bundle_content();

        if bundle_len <= max_bundle_content {
            // Fits in a single Bundle message.
            self.pending.push_back(Message::Bundle {
                hints: vec![],
                data,
            });
            self.wake_drain();
            return Ok(None);
        }

        // Segment the bundle.
        let transfer_number = self.allocator.allocate()?;
        let segment_data_capacity = self.max_segment_data();
        let bundle_length_hint = vec![HintItem::BundleLength(bundle_len as u64)];
        let first_segment_hint_len = hint::encoded_hints_len(&bundle_length_hint);
        let first_segment_data_capacity = self
            .config
            .pdu_size
            .saturating_sub(HEADER_SIZE + 8 + first_segment_hint_len);

        if segment_data_capacity == 0 || first_segment_data_capacity == 0 {
            // PDU too small to carry a segment with the bundle-length hint.
            self.allocator.release();
            return Err(Error::PduOverflow {
                message_size: HEADER_SIZE + 8 + first_segment_hint_len,
                remaining: self.config.pdu_size,
            });
        }

        let mut offset = 0;
        let mut segment_index: u32 = 0;

        while offset < bundle_len {
            let capacity = if segment_index == 0 {
                first_segment_data_capacity
            } else {
                segment_data_capacity
            };
            let remaining = bundle_len - offset;
            let is_last = remaining <= capacity;
            let chunk_size = remaining.min(capacity);
            let segment_data = data.slice(offset..offset + chunk_size);
            offset += chunk_size;

            // Attach the bundle length hint to the first segment.
            let hints = if segment_index == 0 {
                bundle_length_hint.clone()
            } else {
                vec![]
            };

            if is_last {
                self.pending
                    .push_back(Message::TransferEnd(TransferEndMessage {
                        transfer_number,
                        segment_index,
                        hints,
                        data: segment_data,
                    }));
            } else {
                self.pending
                    .push_back(Message::TransferSegment(TransferSegmentMessage {
                        transfer_number,
                        segment_index,
                        hints,
                        data: segment_data,
                    }));
            }
            segment_index += 1;
        }

        self.wake_drain();
        Ok(Some(transfer_number))
    }

    /// Emit a Transfer Cancel message for the given transfer number.
    pub fn cancel(&mut self, transfer_number: u32) {
        // Remove any pending messages for this transfer.
        self.pending
            .retain(|m| !is_transfer_message(m, transfer_number));
        self.pending
            .push_back(Message::TransferCancel { transfer_number });
        self.allocator.release();
        // A slot freed and a new message was pushed.
        self.wake_enqueue();
        self.wake_drain();
    }

    /// Pack pending messages into a PDU buffer of `pdu_size` bytes.
    ///
    /// Returns `None` if no messages are pending.  The returned buffer is
    /// padded to exactly `pdu_size` bytes.
    pub fn next_pdu(&mut self) -> Option<BytesMut> {
        if self.pending.is_empty() {
            return None;
        }

        let pdu_size = self.config.pdu_size;
        let mut buf = BytesMut::with_capacity(pdu_size);

        while !self.pending.is_empty() {
            let msg = self.pending.front().unwrap();
            let msg_len = codec::encoded_message_len(msg);
            if buf.len() + msg_len > pdu_size {
                break;
            }
            let msg = self.pending.pop_front().unwrap();
            // encode_message should not fail for well-formed messages.
            codec::encode_message(&msg, &mut buf)
                .expect("encode_message failed for well-formed message");
        }

        codec::pad_pdu(&mut buf, pdu_size);
        Some(buf)
    }

    /// Returns `true` if there are messages pending for transmission.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Mark a transfer as complete, freeing its window slot.
    pub fn complete(&mut self, _transfer_number: u32) {
        self.allocator.release();
        self.wake_enqueue();
    }

    // -- helpers ------------------------------------------------------------

    /// Maximum content size for a Bundle message that fits in one PDU.
    fn max_single_bundle_content(&self) -> usize {
        self.config.pdu_size.saturating_sub(HEADER_SIZE)
    }

    /// Maximum segment data bytes per Transfer Segment/End message.
    ///
    /// Each segment message has: header (4) + transfer_number (4) +
    /// segment_index (4) = 12 bytes of overhead (ignoring hints on
    /// non-first segments).
    fn max_segment_data(&self) -> usize {
        self.config.pdu_size.saturating_sub(HEADER_SIZE + 8)
    }
}

fn is_transfer_message(msg: &Message, transfer_number: u32) -> bool {
    match msg {
        Message::TransferSegment(m) => m.transfer_number == transfer_number,
        Message::TransferEnd(m) => m.transfer_number == transfer_number,
        Message::TransferCancel { transfer_number: t } => *t == transfer_number,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn make_sender(pdu_size: usize) -> Sender {
        Sender::new(
            SenderConfig {
                pdu_size,
                window_size: 16,
            },
            0,
        )
    }

    #[test]
    fn small_bundle_no_segmentation() {
        let mut s = make_sender(256);
        let data = Bytes::from_static(b"hello");
        let result = s.enqueue(data).unwrap();
        assert_eq!(result, None); // No transfer number = Bundle message

        let pdu = s.next_pdu().unwrap();
        assert_eq!(pdu.len(), 256);

        let messages = codec::decode_pdu(pdu.clone().freeze()).unwrap();
        // Should contain the Bundle message plus padding
        let bundles: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m, Message::Bundle { .. }))
            .collect();
        assert_eq!(bundles.len(), 1);
        match &bundles[0] {
            Message::Bundle { data, .. } => assert_eq!(data.as_ref(), b"hello"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn large_bundle_segmented() {
        let pdu_size = 32;
        let mut s = make_sender(pdu_size);
        // Create a bundle larger than what fits in one PDU
        let data = Bytes::from(vec![0xAB; 100]);
        let result = s.enqueue(data.clone()).unwrap();
        assert!(result.is_some()); // Should be segmented

        // Collect all PDUs
        let mut all_messages = Vec::new();
        while s.has_pending() {
            let pdu = s.next_pdu().unwrap();
            assert_eq!(pdu.len(), pdu_size);
            let msgs = codec::decode_pdu(pdu.clone().freeze()).unwrap();
            all_messages.extend(msgs);
        }

        // Should have TransferSegment(s) + one TransferEnd
        let segments: Vec<_> = all_messages
            .iter()
            .filter(|m| matches!(m, Message::TransferSegment(_)))
            .collect();
        let ends: Vec<_> = all_messages
            .iter()
            .filter(|m| matches!(m, Message::TransferEnd(_)))
            .collect();
        assert!(!segments.is_empty());
        assert_eq!(ends.len(), 1);

        // Verify segment indices are sequential
        let mut indices: Vec<u32> = segments
            .iter()
            .filter_map(|m| match m {
                Message::TransferSegment(s) => Some(s.segment_index),
                _ => None,
            })
            .collect();
        if let Message::TransferEnd(e) = &ends[0] {
            indices.push(e.segment_index);
        }
        let expected: Vec<u32> = (0..indices.len() as u32).collect();
        assert_eq!(indices, expected);

        // Verify reassembly produces original data
        let mut reassembled = Vec::new();
        for msg in &all_messages {
            match msg {
                Message::TransferSegment(s) => reassembled.push((s.segment_index, s.data.clone())),
                Message::TransferEnd(e) => reassembled.push((e.segment_index, e.data.clone())),
                _ => {}
            }
        }
        reassembled.sort_by_key(|(idx, _)| *idx);
        let combined: Vec<u8> = reassembled
            .into_iter()
            .flat_map(|(_, d)| d.to_vec())
            .collect();
        assert_eq!(combined, data.to_vec());
    }

    #[test]
    fn cancel_removes_pending() {
        let mut s = make_sender(32);
        let data = Bytes::from(vec![0; 200]);
        let tn = s.enqueue(data).unwrap().unwrap();
        assert!(s.has_pending());

        s.cancel(tn);
        // Should have only the TransferCancel message now
        let pdu = s.next_pdu().unwrap();
        let msgs = codec::decode_pdu(pdu.clone().freeze()).unwrap();
        let cancels: Vec<_> = msgs
            .iter()
            .filter(|m| matches!(m, Message::TransferCancel { .. }))
            .collect();
        assert_eq!(cancels.len(), 1);
    }

    #[test]
    fn no_pending_returns_none() {
        let mut s = make_sender(256);
        assert!(s.next_pdu().is_none());
    }

    #[test]
    fn window_exhaustion() {
        let mut s = Sender::new(
            SenderConfig {
                pdu_size: 32,
                window_size: 4,
            },
            0,
        );
        for _ in 0..4 {
            s.enqueue(Bytes::from(vec![0; 100])).unwrap();
        }
        // 5th should fail
        let result = s.enqueue(Bytes::from(vec![0; 100]));
        assert!(result.is_err());
    }

    #[test]
    fn first_segment_has_bundle_length_hint() {
        let mut s = make_sender(32);
        let data = Bytes::from(vec![0xCC; 80]);
        s.enqueue(data.clone()).unwrap();

        let pdu = s.next_pdu().unwrap();
        let msgs = codec::decode_pdu(pdu.clone().freeze()).unwrap();

        // The first message should be a TransferSegment with a BundleLength hint
        let first_segment = msgs
            .iter()
            .find(|m| matches!(m, Message::TransferSegment(_)));
        if let Some(Message::TransferSegment(seg)) = first_segment {
            assert_eq!(seg.segment_index, 0);
            assert!(
                seg.hints
                    .iter()
                    .any(|h| matches!(h, HintItem::BundleLength(80)))
            );
        } else {
            panic!("Expected first message to be a TransferSegment");
        }
    }
}
