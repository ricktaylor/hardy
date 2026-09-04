use alloc::{collections::VecDeque, vec, vec::Vec};
use core::num::NonZeroUsize;

use bytes::{Bytes, BytesMut};

use crate::{
    codec::{
        self,
        header::{HEADER_SIZE, MAX_CONTENT_LENGTH},
        hint::{self, HintItem},
        message::{Message, TransferEndMessage, TransferSegmentMessage},
    },
    transfer::{TransferNumberAllocator, WindowSize},
};

/// Errors from PDU sizing and enqueuing bundles for transmission.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The PDU size is outside [`PduSize::MIN`]..=[`PduSize::MAX`].
    #[error(
        "Invalid PDU size {0} (must be {min}..={max})",
        min = PduSize::MIN,
        max = PduSize::MAX
    )]
    InvalidPduSize(usize),

    /// The bundle is empty.  A Bundle Message's content MUST be a valid
    /// bundle (Section 8.1), which an empty payload can never be; rejecting
    /// it here keeps the sender from emitting the zero-content Bundle
    /// Message the draft says SHOULD NOT be used.
    #[error("Empty bundle")]
    EmptyBundle,

    /// The send queue depth is zero.
    #[error("Invalid send queue depth {0} (must be non-zero)")]
    InvalidSendQueueDepth(usize),

    /// A caller-supplied hint item is not encodable (type or value out of
    /// range).  Checked at [`Sender::enqueue`] so the fault is attributed to
    /// the offending bundle rather than surfacing later during PDU packing.
    #[error(transparent)]
    InvalidHint(#[from] crate::codec::Error),

    /// No transfer window slot is available.
    #[error(transparent)]
    Window(#[from] crate::transfer::Error),

    /// A message does not fit in the remaining PDU space.
    #[error(
        "PDU overflow: message of {message_size} bytes exceeds remaining PDU space of {remaining}"
    )]
    PduOverflow {
        message_size: usize,
        remaining: usize,
    },
}

/// A validated convergence layer PDU size in bytes
/// ([`PduSize::MIN`]..=[`PduSize::MAX`]).
///
/// Construct via [`TryFrom<usize>`], which enforces the bounds at the edge;
/// every consumer of a `PduSize` can then rely on them.  Together they
/// guarantee that anything [`Sender::enqueue`] accepts can eventually be
/// drained by [`Sender::next_pdu`]: no queued message is ever larger than a
/// PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct PduSize(usize);

impl PduSize {
    /// Minimum supported convergence layer PDU size in bytes: one message
    /// header.
    ///
    /// Below this, `enqueue` could accept a message (the header alone is
    /// [`HEADER_SIZE`] bytes) that no PDU can ever carry, and `next_pdu`
    /// would emit pure padding forever without draining it.
    pub const MIN: usize = HEADER_SIZE;

    /// Maximum supported convergence layer PDU size in bytes.
    ///
    /// A PDU of this size can be exactly filled by a single message carrying
    /// the maximum 20-bit content length.  Any message or padding content the
    /// sender derives from a `PduSize` is guaranteed encodable; beyond this
    /// bound, segment capacities and padding lengths would overflow the
    /// 20-bit length field.
    pub const MAX: usize = HEADER_SIZE + MAX_CONTENT_LENGTH;

    /// Returns the PDU size as a plain integer.
    pub fn get(self) -> usize {
        self.0
    }
}

impl Default for PduSize {
    /// A common Ethernet-MTU-sized PDU (1500 bytes).
    fn default() -> Self {
        Self(1500)
    }
}

impl TryFrom<usize> for PduSize {
    type Error = Error;

    fn try_from(v: usize) -> Result<Self, Self::Error> {
        if (Self::MIN..=Self::MAX).contains(&v) {
            Ok(Self(v))
        } else {
            Err(Error::InvalidPduSize(v))
        }
    }
}

impl From<PduSize> for usize {
    fn from(p: PduSize) -> usize {
        p.0
    }
}

/// A validated bound on the sender's pending-message queue (non-zero).
///
/// Construct via [`TryFrom<usize>`], which enforces the bound at the edge;
/// a zero depth would park the `tower` `Service::poll_ready` forever.  The
/// depth is counted in messages, and every pending message is at most
/// `pdu_size` bytes, so buffered memory is bounded by roughly
/// `depth * pdu_size`.
///
/// The bound drives backpressure, not errors: `Service::poll_ready` returns
/// `Pending` while the queue is at depth, and direct [`Sender::enqueue`]
/// callers pace themselves by draining [`Sender::next_pdu`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "usize", into = "usize")
)]
pub struct SendQueueDepth(NonZeroUsize);

impl SendQueueDepth {
    /// Returns the depth as a plain integer.
    pub fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for SendQueueDepth {
    /// 64 pending messages — about 96 KiB of buffered PDUs at the default
    /// 1500-byte [`PduSize`].
    fn default() -> Self {
        Self(NonZeroUsize::new(64).unwrap())
    }
}

impl TryFrom<usize> for SendQueueDepth {
    type Error = Error;

    fn try_from(v: usize) -> Result<Self, Self::Error> {
        NonZeroUsize::new(v)
            .map(Self)
            .ok_or(Error::InvalidSendQueueDepth(v))
    }
}

impl From<SendQueueDepth> for usize {
    fn from(d: SendQueueDepth) -> usize {
        d.0.get()
    }
}

/// Options for [`Sender::enqueue`].
///
/// `Default`-constructible; `SendOpts::default()` sends with no
/// caller-supplied hints.
///
/// Deliberately **not** `#[non_exhaustive]`: `hints` is already the
/// catch-all — any metadata the sender needn't specially understand rides
/// through as [`HintItem::Unknown`].  A new structured field is only ever
/// added for a capability the sender actively implements (a priority
/// selector, a repetition policy), which is a coordinated change to this
/// crate and its callers together; the compile break on the struct literal
/// is the useful checklist.
#[derive(Debug, Clone, Default)]
pub struct SendOpts {
    /// Hint items to attach to the transfer, carried on the Bundle message
    /// (unsegmented) or the first segment (segmented) alongside the
    /// sender-derived Bundle Length hint.  A caller-supplied
    /// [`HintItem::BundleLength`] is discarded: the sender derives the
    /// truthful value itself.
    pub hints: Vec<HintItem>,
}

/// A bundle plus its [`SendOpts`], the request type of the hint-capable
/// `tower` `Service` impl.  `Service<Bytes>` remains available as a
/// default-options convenience.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub data: Bytes,
    pub opts: SendOpts,
}

/// How [`Sender::enqueue`] queued a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// Sent as a single Bundle Message; no transfer state to track.
    Bundle,
    /// Segmented; [`Sender::complete`] or [`Sender::cancel`] must eventually
    /// be called with this transfer number to free its window slot.
    Transfer(u32),
}

/// Manages outbound BTP-U transfers, segmentation, and PDU packing.
///
/// The sender is convergence-layer agnostic: a CLA calls [`Sender::enqueue`] to
/// submit bundles and [`Sender::next_pdu`] to obtain packed PDU buffers
/// ready for transmission.
///
/// # Bundle framing
///
/// A bundle that fits in one PDU is always emitted as a type-2 Bundle
/// message (Section 8.1): the 4-byte header is the only overhead, and it is
/// what lets a small bundle share a PDU with another transfer's segments,
/// be followed by padding on a fixed-size link, and carry hints.  The sender
/// never emits a bare bundle frame (a PDU whose first byte is a
/// bundle-reserved type, Section 12.1).  Those reserved values guarantee that
/// a *receiver* can tell such a frame from BTP-U messages, and
/// [`decode_pdu`](crate::codec::decode_pdu) accepts them, but emitting one is
/// a per-link decision the CLA makes before calling [`Sender::enqueue`]: on a
/// variable-length link to a peer that accepts bare bundles, write the
/// bundle bytes straight to the link when they fit and enqueue here only
/// when they need segmentation.  A bare frame cannot be packed, padded, or
/// hinted, so it has no place in the PDU packer.
///
/// # Concurrency
///
/// `Sender` is designed for **single-owner** use: one task at a time mutates
/// it via `&mut self`. The `tower::Service` and `futures_core::Stream` impls
/// (under the `tower` feature) follow this contract. To share a `Sender`
/// across tasks, wrap it in `Arc<Mutex<_>>`; the outer synchronisation
/// serialises both the state and the waker registers kept inside.  Do
/// **not** use `tower::buffer::Buffer`: it moves the `Sender` into a worker
/// task and exposes only the `Service` half, stranding the `Stream` drain
/// and [`Self::complete`]/[`Self::cancel`] — PDUs never leave and window
/// slots never free.
pub struct Sender {
    pdu_size: PduSize,
    /// Backpressure bound on `pending`, in messages; enforced by the `tower`
    /// `Service::poll_ready` rather than by `enqueue` itself.
    send_queue_depth: SendQueueDepth,
    /// Owns the set of outstanding transfer numbers and the Section 5 window
    /// rule.  [`Self::complete`] and [`Self::cancel`] only act on numbers it
    /// reports as outstanding, so bogus or duplicate calls cannot free slots
    /// that were never allocated.
    allocator: TransferNumberAllocator,
    pending: VecDeque<Message>,
    /// Wakers stored by the `tower` Service/Stream impls. `enqueue_waker` is
    /// woken when a window slot frees or the send queue drains below its
    /// depth; `drain_waker` is woken when a new
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
    pub fn new(
        pdu_size: PduSize,
        window_size: WindowSize,
        send_queue_depth: SendQueueDepth,
        initial_transfer_number: u32,
    ) -> Self {
        Self {
            pdu_size,
            send_queue_depth,
            allocator: TransferNumberAllocator::new(window_size, initial_transfer_number),
            pending: VecDeque::new(),
            #[cfg(feature = "tower")]
            enqueue_waker: None,
            #[cfg(feature = "tower")]
            drain_waker: None,
        }
    }

    /// Create a new sender with the initial transfer number seeded from `rng`.
    /// Convenience wrapper over [`Self::new`].
    #[cfg(feature = "rand")]
    pub fn from_rng<R: rand_core::Rng>(
        pdu_size: PduSize,
        window_size: WindowSize,
        send_queue_depth: SendQueueDepth,
        rng: &mut R,
    ) -> Self {
        Self::new(pdu_size, window_size, send_queue_depth, rng.next_u32())
    }

    /// Wake any task parked on a `Service::poll_ready` that returned
    /// `Pending` because the window was full or the send queue was at
    /// depth. No-op without the `tower` feature.
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

    /// Whether a segmented bundle could currently be admitted without
    /// violating the transfer window (see
    /// [`TransferNumberAllocator::can_allocate`]).
    ///
    /// The `tower` `Service::poll_ready` uses this as its window gate, so it
    /// is exactly the predicate [`Self::enqueue`] applies when segmenting.
    pub fn window_available(&self) -> bool {
        self.allocator.can_allocate()
    }

    /// Whether the pending-message queue has reached its configured
    /// [`SendQueueDepth`].
    ///
    /// The `tower` `Service::poll_ready` uses this as its admission gate:
    /// unsegmented bundles take no window slot, so without it the queue
    /// would grow without bound whenever the drain side is slower.  Direct
    /// [`Self::enqueue`] callers can poll it to pace themselves the same
    /// way, draining [`Self::next_pdu`] when it reports full.
    pub fn send_queue_full(&self) -> bool {
        self.pending.len() >= self.send_queue_depth.get()
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
    /// without segmentation and [`Enqueued::Bundle`] is returned.  Otherwise,
    /// it is split into Transfer Segment and Transfer End messages and
    /// [`Enqueued::Transfer`] carries the allocated transfer number.
    ///
    /// Caller hints from `opts` ride on the Bundle message or the first
    /// segment (hints are transfer-scoped, Section 7.2); the sender derives
    /// and attaches the Bundle Length hint itself when segmenting.
    ///
    /// An empty `data` is rejected with [`Error::EmptyBundle`]: it cannot be
    /// a valid bundle (Section 8.1), and nothing is queued.
    pub fn enqueue(&mut self, data: Bytes, opts: SendOpts) -> Result<Enqueued, Error> {
        if data.is_empty() {
            return Err(Error::EmptyBundle);
        }

        // Validate up front: a bad hint must fail this bundle, not panic
        // later when next_pdu encodes the queued message.  The sender owns
        // the Bundle Length hint, so a caller-supplied one is discarded.
        let mut caller_hints = opts.hints;
        caller_hints.retain(|h| !matches!(h, HintItem::BundleLength(_)));
        hint::validate_hints(&caller_hints)?;

        let bundle_len = data.len();
        let caller_hints_len = hint::encoded_hints_len(&caller_hints);
        let max_bundle_content = self.max_single_bundle_content();

        if bundle_len + caller_hints_len <= max_bundle_content {
            // Fits in a single Bundle message.
            self.pending.push_back(Message::Bundle {
                hints: caller_hints,
                data,
            });
            self.wake_drain();
            return Ok(Enqueued::Bundle);
        }

        // Segment the bundle.  Size the segments before taking a transfer
        // number so a PDU too small to carry them never touches the window.
        let segment_data_capacity = self.max_segment_data();
        let mut first_segment_hints = vec![HintItem::BundleLength(bundle_len as u64)];
        first_segment_hints.extend(caller_hints);
        let first_segment_hint_len = hint::encoded_hints_len(&first_segment_hints);
        let first_segment_data_capacity = self
            .pdu_size
            .get()
            .saturating_sub(HEADER_SIZE + 8 + first_segment_hint_len);

        if segment_data_capacity == 0 || first_segment_data_capacity == 0 {
            // PDU too small to carry a segment with the bundle-length hint.
            return Err(Error::PduOverflow {
                message_size: HEADER_SIZE + 8 + first_segment_hint_len,
                remaining: self.pdu_size.get(),
            });
        }

        let transfer_number = self.allocator.allocate()?;

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

            // Attach the bundle-length and caller hints to the first segment.
            let hints = if segment_index == 0 {
                first_segment_hints.clone()
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
        Ok(Enqueued::Transfer(transfer_number))
    }

    /// Emit a Transfer Cancel message for the given transfer number.
    ///
    /// A no-op if `transfer_number` is not an outstanding transfer of this
    /// sender (never allocated, already completed, or already cancelled):
    /// no Cancel message is queued and no window slot is released.
    pub fn cancel(&mut self, transfer_number: u32) {
        if !self.allocator.release(transfer_number) {
            return;
        }

        // Remove any pending messages for this transfer.
        self.pending
            .retain(|m| !is_transfer_message(m, transfer_number));
        self.pending
            .push_back(Message::TransferCancel { transfer_number });
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

        let pdu_size = self.pdu_size.get();
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

        // Draining frees send-queue capacity; wake any task parked on
        // `poll_ready` because the queue was at depth.
        self.wake_enqueue();

        codec::pad_pdu(&mut buf, pdu_size);
        Some(buf)
    }

    /// Returns `true` if there are messages pending for transmission.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Mark a transfer as complete, freeing its window slot.
    ///
    /// A no-op if `transfer_number` is not an outstanding transfer of this
    /// sender, so duplicate or bogus calls cannot free slots that were never
    /// allocated.  Note that the window only advances when the *oldest*
    /// outstanding transfer completes (Section 5); completing a newer one
    /// frees no capacity until every older transfer has also finished.
    pub fn complete(&mut self, transfer_number: u32) {
        if !self.allocator.release(transfer_number) {
            return;
        }
        self.wake_enqueue();
    }

    // -- helpers ------------------------------------------------------------

    /// Maximum content size for a Bundle message that fits in one PDU.
    fn max_single_bundle_content(&self) -> usize {
        self.pdu_size.get().saturating_sub(HEADER_SIZE)
    }

    /// Maximum segment data bytes per Transfer Segment/End message.
    ///
    /// Each segment message has: header (4) + transfer_number (4) +
    /// segment_index (4) = 12 bytes of overhead (ignoring hints on
    /// non-first segments).
    fn max_segment_data(&self) -> usize {
        self.pdu_size.get().saturating_sub(HEADER_SIZE + 8)
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

    fn make_sender(pdu_size: usize) -> Sender {
        sender_with(pdu_size, 16)
    }

    /// Unwrap the segmented case; panics on [`Enqueued::Bundle`].
    fn transfer_number(e: Enqueued) -> u32 {
        match e {
            Enqueued::Transfer(t) => t,
            Enqueued::Bundle => panic!("expected a segmented transfer"),
        }
    }

    fn sender_with(pdu_size: usize, window_size: u16) -> Sender {
        Sender::new(
            PduSize::try_from(pdu_size).unwrap(),
            WindowSize::try_from(window_size).unwrap(),
            SendQueueDepth::default(),
            0,
        )
    }

    #[test]
    fn max_pdu_size_accepted() {
        make_sender(PduSize::MAX);
    }

    #[test]
    fn oversized_pdu_size_rejected() {
        assert!(matches!(
            PduSize::try_from(PduSize::MAX + 1),
            Err(Error::InvalidPduSize(v)) if v == PduSize::MAX + 1
        ));
    }

    #[test]
    fn pdu_size_below_one_header_rejected() {
        assert_eq!(PduSize::MIN, HEADER_SIZE);
        assert_eq!(PduSize::try_from(PduSize::MIN).unwrap().get(), HEADER_SIZE);
        assert!(matches!(
            PduSize::try_from(PduSize::MIN - 1),
            Err(Error::InvalidPduSize(v)) if v == PduSize::MIN - 1
        ));
        assert!(matches!(
            PduSize::try_from(0),
            Err(Error::InvalidPduSize(0))
        ));
    }

    #[test]
    fn empty_bundle_rejected_at_enqueue() {
        let mut s = make_sender(256);
        assert!(matches!(
            s.enqueue(Bytes::new(), SendOpts::default()),
            Err(Error::EmptyBundle)
        ));
        assert!(!s.has_pending());
        assert!(s.next_pdu().is_none());
    }

    #[test]
    fn minimum_pdu_size_cannot_queue_an_undrainable_message() {
        // At PduSize::MIN only a zero-content Bundle message would fit, and
        // empty bundles are rejected; a one-byte bundle must take the
        // segmentation path and fail cleanly rather than queue a message
        // larger than any PDU (which would make the drain loop spin).
        let mut s = make_sender(PduSize::MIN);
        assert!(matches!(
            s.enqueue(Bytes::from_static(b"x"), SendOpts::default()),
            Err(Error::PduOverflow { .. })
        ));
        assert!(!s.has_pending());
        assert!(s.next_pdu().is_none());
    }

    #[test]
    fn send_queue_depth_zero_rejected() {
        assert!(matches!(
            SendQueueDepth::try_from(0),
            Err(Error::InvalidSendQueueDepth(0))
        ));
        assert_eq!(SendQueueDepth::try_from(1).unwrap().get(), 1);
        assert_eq!(SendQueueDepth::default().get(), 64);
    }

    #[test]
    fn caller_hints_ride_first_segment_with_derived_bundle_length() {
        let mut s = sender_with(64, 4);
        let correlator = HintItem::Unknown {
            hint_type: 0x41,
            value: Bytes::from_static(b"\x01\x02"),
        };
        // The caller-supplied BundleLength is discarded; the sender derives
        // the truthful one and puts it first.
        let opts = SendOpts {
            hints: vec![HintItem::BundleLength(999), correlator.clone()],
        };
        s.enqueue(Bytes::from(vec![0u8; 200]), opts).unwrap();

        let pdu = s.next_pdu().unwrap().freeze();
        let first = codec::decode_pdu(pdu).next().unwrap().unwrap();
        let Message::TransferSegment(m) = first else {
            panic!("expected a leading Transfer Segment, got {first:?}")
        };
        assert_eq!(m.hints, vec![HintItem::BundleLength(200), correlator]);
        // The first segment's data capacity shrinks by exactly the merged
        // hint chain's encoded size.
        assert_eq!(
            m.data.len(),
            64 - HEADER_SIZE - 8 - hint::encoded_hints_len(&m.hints)
        );
    }

    #[test]
    fn first_segment_capacity_reduced_by_exactly_the_hint_bytes() {
        let pdu_size = 32;
        let mut s = make_sender(pdu_size);
        s.enqueue(Bytes::from(vec![0xAB; 100]), SendOpts::default())
            .unwrap();

        let mut messages = Vec::new();
        while s.has_pending() {
            let pdu = s.next_pdu().unwrap().freeze();
            messages.extend(
                codec::decode_pdu(pdu)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
            );
        }

        // Every segment carries a fixed overhead of the 4-byte message
        // header plus the 8-byte transfer number + segment index prefix.
        let full_capacity = pdu_size - HEADER_SIZE - 8;

        let mut segments: Vec<&TransferSegmentMessage> = messages
            .iter()
            .filter_map(|m| match m {
                Message::TransferSegment(seg) => Some(seg),
                _ => None,
            })
            .collect();
        segments.sort_by_key(|seg| seg.segment_index);
        let (first, middle) = segments.split_first().unwrap();
        assert_eq!(first.segment_index, 0);

        // The first segment cedes exactly the encoded hint bytes to the
        // Bundle Length hint...
        let hint_len = hint::encoded_hints_len(&first.hints);
        assert!(hint_len > 0);
        assert_eq!(first.data.len(), full_capacity - hint_len);

        // ...while every hintless middle segment fills its PDU exactly.
        assert!(!middle.is_empty());
        for seg in middle {
            assert!(seg.hints.is_empty());
            assert_eq!(
                seg.data.len(),
                full_capacity,
                "segment {}",
                seg.segment_index
            );
        }
    }

    #[test]
    fn caller_hints_ride_unsegmented_bundle_message() {
        let mut s = sender_with(64, 4);
        let hint = HintItem::Unknown {
            hint_type: 0x41,
            value: Bytes::from_static(b"z"),
        };
        let e = s
            .enqueue(
                Bytes::from_static(b"tiny"),
                SendOpts {
                    hints: vec![hint.clone()],
                },
            )
            .unwrap();
        assert_eq!(e, Enqueued::Bundle);

        let pdu = s.next_pdu().unwrap().freeze();
        let msg = codec::decode_pdu(pdu).next().unwrap().unwrap();
        let Message::Bundle { hints, data } = msg else {
            panic!("expected a Bundle message, got {msg:?}")
        };
        assert_eq!(hints, vec![hint]);
        assert_eq!(data.as_ref(), b"tiny");
    }

    #[test]
    fn invalid_caller_hint_rejected_at_enqueue() {
        let mut s = sender_with(64, 4);
        let bad = HintItem::Unknown {
            hint_type: 0x80,
            value: Bytes::from_static(b"x"),
        };
        let err = s
            .enqueue(Bytes::from_static(b"tiny"), SendOpts { hints: vec![bad] })
            .unwrap_err();
        assert!(matches!(
            err,
            Error::InvalidHint(crate::codec::Error::InvalidHintType(0x80))
        ));
        // The bad bundle queued nothing.
        assert!(!s.has_pending());
    }

    #[test]
    fn max_pdu_size_bundle_encodes_without_panic() {
        // Regression: with pdu_size at the limit, both the largest possible
        // Bundle message and the segmentation path must stay within the
        // 20-bit content length -- next_pdu must never hit its expect().
        let mut s = make_sender(PduSize::MAX);

        // Largest bundle that fits unsegmented: content == MAX_CONTENT_LENGTH.
        s.enqueue(
            Bytes::from(alloc::vec![0u8; MAX_CONTENT_LENGTH]),
            SendOpts::default(),
        )
        .unwrap();
        // One byte more: must segment, and every segment must encode.
        s.enqueue(
            Bytes::from(alloc::vec![0u8; MAX_CONTENT_LENGTH + 1]),
            SendOpts::default(),
        )
        .unwrap();

        let mut pdus = 0;
        while let Some(pdu) = s.next_pdu() {
            assert_eq!(pdu.len(), PduSize::MAX);
            pdus += 1;
        }
        assert!(pdus >= 2);
    }

    #[test]
    fn small_bundle_no_segmentation() {
        let mut s = make_sender(256);
        let data = Bytes::from_static(b"hello");
        let result = s.enqueue(data, SendOpts::default()).unwrap();
        assert_eq!(result, Enqueued::Bundle);

        let pdu = s.next_pdu().unwrap();
        assert_eq!(pdu.len(), 256);

        let messages: Vec<_> = codec::decode_pdu(pdu.clone().freeze())
            .collect::<Result<_, _>>()
            .unwrap();
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
        let result = s.enqueue(data.clone(), SendOpts::default()).unwrap();
        assert!(matches!(result, Enqueued::Transfer(_)));

        // Collect all PDUs
        let mut all_messages = Vec::new();
        while s.has_pending() {
            let pdu = s.next_pdu().unwrap();
            assert_eq!(pdu.len(), pdu_size);
            let msgs: Vec<_> = codec::decode_pdu(pdu.clone().freeze())
                .collect::<Result<_, _>>()
                .unwrap();
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

    /// A deterministic RNG for testing `from_rng`.  Implemented against the
    /// `rand` crate's `rand_core` re-export, proving this crate's `rand_core`
    /// version actually lines up with the `rand` in use.
    #[cfg(feature = "rand")]
    struct FixedRng(u32);

    #[cfg(feature = "rand")]
    impl rand::rand_core::TryRng for FixedRng {
        type Error = core::convert::Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            Ok(self.0)
        }
        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            Ok(self.0 as u64)
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
            dst.fill(0);
            Ok(())
        }
    }

    #[cfg(feature = "rand")]
    #[test]
    fn from_rng_seeds_initial_transfer_number() {
        let mut s = Sender::from_rng(
            PduSize::try_from(64).unwrap(),
            WindowSize::default(),
            SendQueueDepth::default(),
            &mut FixedRng(0xDEAD_BEEF),
        );
        let t = transfer_number(
            s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default())
                .unwrap(),
        );
        assert_eq!(t, 0xDEAD_BEEF);
    }

    #[test]
    fn bogus_complete_does_not_free_slot() {
        let mut s = sender_with(64, 4);
        // Saturate the window with segmented transfers.
        for _ in 0..4 {
            transfer_number(
                s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default())
                    .unwrap(),
            );
        }
        assert!(
            s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default())
                .is_err()
        );

        // A transfer number never allocated must not free a slot.
        s.complete(999);
        assert!(
            s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default())
                .is_err()
        );

        // Completing a real transfer frees exactly one slot...
        s.complete(0);
        let t = transfer_number(
            s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default())
                .unwrap(),
        );
        assert_eq!(t, 4);
        assert!(
            s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default())
                .is_err()
        );

        // ...and a duplicate complete of the same transfer frees nothing.
        s.complete(0);
        assert!(
            s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default())
                .is_err()
        );
    }

    #[test]
    fn out_of_order_completion_keeps_active_span_within_window() {
        // Section 5: the sender MUST NOT emit a message whose transfer
        // number is <= greatest - window_size.  Completing the newest
        // transfer while the oldest is still active must not admit a new
        // number, or a later Cancel for the oldest would violate the rule
        // (and a reordered End for it would land outside the receiver's
        // window).
        let mut s = sender_with(64, 4);
        let bundle = || Bytes::from(vec![0; 200]);
        let mut numbers = Vec::new();
        for _ in 0..4 {
            numbers.push(transfer_number(
                s.enqueue(bundle(), SendOpts::default()).unwrap(),
            ));
        }
        assert_eq!(numbers, vec![0, 1, 2, 3]);

        s.complete(3);
        assert!(!s.window_available());
        assert!(matches!(
            s.enqueue(bundle(), SendOpts::default()),
            Err(Error::Window(crate::transfer::Error::WindowFull {
                window_size: 4
            }))
        ));

        // Releasing the oldest advances the window base; 4 is now admissible
        // and every still-active number (1, 2) stays within 4 - 4 + 1..=4.
        s.complete(0);
        assert!(s.window_available());
        let t = transfer_number(s.enqueue(bundle(), SendOpts::default()).unwrap());
        assert_eq!(t, 4);

        // Cancelling an in-window active transfer still emits its Cancel.
        s.cancel(1);
        assert!(
            s.pending
                .iter()
                .any(|m| matches!(m, Message::TransferCancel { transfer_number: 1 }))
        );
    }

    #[test]
    fn pdu_too_small_to_segment_leaves_window_untouched() {
        // The PduOverflow check runs before a transfer number is taken, so a
        // failed enqueue neither consumes nor skips a number.
        let mut s = sender_with(12, 4);
        assert!(matches!(
            s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default()),
            Err(Error::PduOverflow { .. })
        ));
        assert_eq!(s.allocator.in_progress(), 0);
        assert!(s.window_available());
        assert!(!s.has_pending());
    }

    #[test]
    fn bogus_cancel_is_noop() {
        let mut s = sender_with(64, 4);
        let tn = transfer_number(
            s.enqueue(Bytes::from(vec![0; 200]), SendOpts::default())
                .unwrap(),
        );

        // Cancel of a never-allocated number: no Cancel message queued, no
        // slot released.
        let pending_before = s.pending.len();
        s.cancel(999);
        assert_eq!(s.pending.len(), pending_before);
        assert_eq!(s.allocator.in_progress(), 1);

        // Real cancel works; repeating it changes nothing further.
        s.cancel(tn);
        assert_eq!(s.allocator.in_progress(), 0);
        let pending_after = s.pending.len();
        s.cancel(tn);
        assert_eq!(s.pending.len(), pending_after);
        assert_eq!(s.allocator.in_progress(), 0);
    }

    #[test]
    fn cancel_removes_pending() {
        let mut s = make_sender(32);
        let data = Bytes::from(vec![0; 200]);
        let tn = transfer_number(s.enqueue(data, SendOpts::default()).unwrap());
        assert!(s.has_pending());

        s.cancel(tn);
        // Should have only the TransferCancel message now
        let pdu = s.next_pdu().unwrap();
        let msgs: Vec<_> = codec::decode_pdu(pdu.clone().freeze())
            .collect::<Result<_, _>>()
            .unwrap();
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
        let mut s = sender_with(32, 4);
        for _ in 0..4 {
            s.enqueue(Bytes::from(vec![0; 100]), SendOpts::default())
                .unwrap();
        }
        // 5th should fail
        let result = s.enqueue(Bytes::from(vec![0; 100]), SendOpts::default());
        assert!(result.is_err());
    }

    #[test]
    fn first_segment_has_bundle_length_hint() {
        let mut s = make_sender(32);
        let data = Bytes::from(vec![0xCC; 80]);
        s.enqueue(data.clone(), SendOpts::default()).unwrap();

        let pdu = s.next_pdu().unwrap();
        let msgs: Vec<_> = codec::decode_pdu(pdu.clone().freeze())
            .collect::<Result<_, _>>()
            .unwrap();

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
