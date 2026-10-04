//! Fixtures shared between the integration-test binaries.

// Each test binary uses its own subset of these helpers, so per-binary
// unused ones are expected.
#![allow(dead_code)]

#[cfg(feature = "rand")]
use core::convert::Infallible;

use bytes::{Bytes, BytesMut};
use hardy_btpu::{
    codec::{
        Error as CodecError, decode_pdu, encode_message,
        header::HEADER_SIZE,
        hint::{HintItem, HintType, HintValue, Hints, encoded_hints_len},
        message::{Message, TransferSegmentMessage},
    },
    receiver::{
        DropReason, MaxSegments, MaxTransferSize, Receiver, ReceiverConfig, ReceiverEvent,
        RejectReason,
    },
    sender::{
        BundleFraming, Carried, Error, LinkFraming, Pdu, PduSize, SendId, SendKind, SendOptions,
        SendQueueHighWatermark, Sender, SenderConfig,
    },
    transfer::{TransferId, WindowSize},
};
#[cfg(feature = "rand")]
use rand::rand_core::TryRng;

// An unknown hint item of type `hint_type` carrying `value`.
pub fn unknown_hint(hint_type: u8, value: &'static [u8]) -> HintItem {
    HintItem::Unknown {
        hint_type: HintType::new(hint_type).unwrap(),
        value: HintValue::new(Bytes::from_static(value)).unwrap(),
    }
}

pub fn window_size(v: u16) -> WindowSize {
    WindowSize::try_from(v).unwrap()
}

pub fn max_transfer_size(v: usize) -> MaxTransferSize {
    MaxTransferSize::try_from(v).unwrap()
}

// A sender with the given PDU and window sizes, default queue depth,
// fixed-size framing, and transfer numbers starting at zero.
pub fn sender(pdu_size: usize, window: u16) -> Sender {
    Sender::new(sender_config(pdu_size, window), 0)
}

pub fn sender_config(pdu_size: usize, window: u16) -> SenderConfig {
    SenderConfig {
        pdu_size: PduSize::try_from(pdu_size).unwrap(),
        window_size: window_size(window),
        ..SenderConfig::default()
    }
}

// As `sender` with a 16-transfer window, and the configuration then
// changed by `f`.
pub fn sender_with(pdu_size: usize, f: impl FnOnce(&mut SenderConfig)) -> Sender {
    let mut config = sender_config(pdu_size, 16);
    f(&mut config);
    Sender::new(config, 0)
}

// As `sender` with a 16-transfer window and the given link framing,
// admitting until `bytes` bundle bytes are queued.
pub fn sender_with_high_watermark(
    pdu_size: usize,
    bytes: usize,
    link_framing: LinkFraming,
) -> Sender {
    sender_with(pdu_size, |c| {
        c.send_queue_high_watermark = SendQueueHighWatermark::try_from(bytes).unwrap();
        c.link_framing = link_framing;
    })
}

// As `sender` with a 16-transfer window on a variable-length link with no
// padding floor.
pub fn variable_sender(pdu_size: usize, bundle_framing: BundleFraming) -> Sender {
    floored_sender(pdu_size, bundle_framing, 0)
}

// As `variable_sender`, padding every PDU up to `min_pdu_len`.
pub fn floored_sender(
    pdu_size: usize,
    bundle_framing: BundleFraming,
    min_pdu_len: usize,
) -> Sender {
    sender_with(pdu_size, |c| {
        c.link_framing = LinkFraming::Variable {
            bundle_framing,
            min_pdu_len,
        };
    })
}

// The transfer number and segment index every segment message carries.
pub const SEGMENT_FIELDS: usize = 8;

// The data bytes segment 0 of a `len`-byte bundle carries in a full PDU.
pub fn first_capacity(pdu_size: usize, len: usize) -> usize {
    pdu_size
        - HEADER_SIZE
        - SEGMENT_FIELDS
        - encoded_hints_len(&[HintItem::BundleLength(len as u64)])
}

// `id`, checked to name a segmented bundle.  A test knows its transfer
// number from the order of allocation: the `sender*` helpers number
// transfers from zero.
pub fn segmented(id: SendId) -> SendId {
    assert_eq!(id.kind(), SendKind::Transfer, "{id:?}");
    id
}

// The transfer numbers whose segments `pdus` carry, in order of first
// appearance.
pub fn wire_transfer_numbers(pdus: &[Bytes]) -> Vec<u32> {
    let mut numbers = Vec::new();
    for message in pdus
        .iter()
        .flat_map(|pdu| decode_pdu(pdu.clone()).map(Result::unwrap))
    {
        if let Message::TransferSegment(m) | Message::TransferEnd(m) = message
            && !numbers.contains(&m.transfer_number)
        {
            numbers.push(m.transfer_number);
        }
    }
    numbers
}

// Enqueue `data` with no caller hints.
pub fn enqueue(s: &mut Sender, data: Bytes) -> Result<SendId, Error> {
    s.enqueue(data, SendOptions::default())
}

// Every PDU `s` has pending, in order, with the bundles each carries.
pub fn drain_pdus(s: &mut Sender) -> Vec<Pdu> {
    let mut pdus = Vec::new();
    while let Some(pdu) = s.next_pdu() {
        pdus.push(pdu);
    }
    pdus
}

// Every PDU `s` has pending, in order.
pub fn drain(s: &mut Sender) -> Vec<Bytes> {
    drain_pdus(s).into_iter().map(|pdu| pdu.data).collect()
}

// Every message in `pdu`, its padding included.
pub fn decode_all(pdu: Bytes) -> Vec<Message> {
    decode_pdu(pdu).collect::<Result<_, _>>().unwrap()
}

pub fn carried(id: SendId, completes: bool) -> Carried {
    Carried { id, completes }
}

// The data `pdus` carry for transfer `t`, in order.
pub fn transfer_data(pdus: &[Bytes], t: u32) -> Vec<u8> {
    pdus.iter()
        .flat_map(|pdu| decode_pdu(pdu.clone()).map(Result::unwrap))
        .filter_map(|m| match m {
            Message::TransferSegment(m) | Message::TransferEnd(m) if m.transfer_number == t => {
                Some(m.data)
            }
            _ => None,
        })
        .flatten()
        .collect()
}

// A receiver with the given window size and bundle-size cap and every
// other setting at its default (FEC decoding off, no segment or retention
// limit).
pub fn receiver(window: u16, max_transfer_size: usize) -> Receiver {
    Receiver::new(receiver_config(window, max_transfer_size))
}

pub fn receiver_config(window: u16, cap: usize) -> ReceiverConfig {
    ReceiverConfig {
        window_size: window_size(window),
        max_transfer_size: max_transfer_size(cap),
        ..ReceiverConfig::default()
    }
}

// As `receiver`, with a per-transfer segment limit.
pub fn receiver_with_max_segments(window: u16, cap: usize, max_segments: MaxSegments) -> Receiver {
    Receiver::new(ReceiverConfig {
        max_segments_per_transfer: Some(max_segments),
        ..receiver_config(window, cap)
    })
}

/// A [`ReceiverEvent`] with each `TransferId` replaced by its wire
/// transfer number, so that a test can spell the events it expects: a
/// `TransferId` has no public constructor.  A `ReceiverEvent` compares
/// equal to the `Event` it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Received {
        data: Bytes,
        hints: Hints,
    },
    TransferStarted {
        transfer_number: u32,
        hints: Hints,
    },
    TransferData {
        transfer_number: u32,
        data: Bytes,
        hints: Option<Hints>,
    },
    TransferFinished {
        transfer_number: u32,
        data: Bytes,
        hints: Option<Hints>,
    },
    TransferCancelled {
        transfer_number: u32,
    },
    TransferExpired {
        transfer_number: u32,
    },
    MessageDropped {
        transfer_number: u32,
        reason: DropReason,
    },
    TransferRejected {
        transfer_number: u32,
        reason: RejectReason,
    },
    BundleRejected {
        len: usize,
    },
    MalformedMessage {
        error: CodecError,
    },
    MalformedPdu {
        error: CodecError,
    },
}

impl From<ReceiverEvent> for Event {
    fn from(event: ReceiverEvent) -> Self {
        match event {
            ReceiverEvent::Received { data, hints } => Self::Received { data, hints },
            ReceiverEvent::TransferStarted { id, hints } => Self::TransferStarted {
                transfer_number: id.transfer_number(),
                hints,
            },
            ReceiverEvent::TransferData { id, data, hints } => Self::TransferData {
                transfer_number: id.transfer_number(),
                data,
                hints,
            },
            ReceiverEvent::TransferFinished { id, data, hints } => Self::TransferFinished {
                transfer_number: id.transfer_number(),
                data,
                hints,
            },
            ReceiverEvent::TransferCancelled { id } => Self::TransferCancelled {
                transfer_number: id.transfer_number(),
            },
            ReceiverEvent::TransferExpired { id } => Self::TransferExpired {
                transfer_number: id.transfer_number(),
            },
            ReceiverEvent::MessageDropped {
                transfer_number,
                id,
                reason,
            } => {
                assert!(drop_id_consistent(transfer_number, id, reason));
                Self::MessageDropped {
                    transfer_number,
                    reason,
                }
            }
            ReceiverEvent::TransferRejected { id, reason } => Self::TransferRejected {
                transfer_number: id.transfer_number(),
                reason,
            },
            ReceiverEvent::BundleRejected { len } => Self::BundleRejected { len },
            ReceiverEvent::MalformedMessage { error } => Self::MalformedMessage { error },
            ReceiverEvent::MalformedPdu { error } => Self::MalformedPdu { error },
        }
    }
}

impl PartialEq<Event> for ReceiverEvent {
    fn eq(&self, other: &Event) -> bool {
        match (self, other) {
            (ReceiverEvent::Received { data, hints }, Event::Received { data: d, hints: h }) => {
                data == d && hints == h
            }
            (
                ReceiverEvent::TransferStarted { id, hints },
                Event::TransferStarted {
                    transfer_number: n,
                    hints: h,
                },
            ) => id.transfer_number() == *n && hints == h,
            (
                ReceiverEvent::TransferData { id, data, hints },
                Event::TransferData {
                    transfer_number: n,
                    data: d,
                    hints: h,
                },
            )
            | (
                ReceiverEvent::TransferFinished { id, data, hints },
                Event::TransferFinished {
                    transfer_number: n,
                    data: d,
                    hints: h,
                },
            ) => id.transfer_number() == *n && data == d && hints == h,
            (
                ReceiverEvent::TransferCancelled { id },
                Event::TransferCancelled { transfer_number: n },
            )
            | (
                ReceiverEvent::TransferExpired { id },
                Event::TransferExpired { transfer_number: n },
            ) => id.transfer_number() == *n,
            (
                ReceiverEvent::MessageDropped {
                    transfer_number,
                    id,
                    reason,
                },
                Event::MessageDropped {
                    transfer_number: n,
                    reason: r,
                },
            ) => {
                transfer_number == n
                    && reason == r
                    && drop_id_consistent(*transfer_number, *id, *reason)
            }
            (
                ReceiverEvent::TransferRejected { id, reason },
                Event::TransferRejected {
                    transfer_number: n,
                    reason: r,
                },
            ) => id.transfer_number() == *n && reason == r,
            (ReceiverEvent::BundleRejected { len }, Event::BundleRejected { len: l }) => len == l,
            (ReceiverEvent::MalformedMessage { error }, Event::MalformedMessage { error: e })
            | (ReceiverEvent::MalformedPdu { error }, Event::MalformedPdu { error: e }) => {
                error == e
            }
            _ => false,
        }
    }
}

// What `ReceiverEvent::MessageDropped` documents of its `id`: none outside
// the receive window, and otherwise one naming the dropped number.
fn drop_id_consistent(transfer_number: u32, id: Option<TransferId>, reason: DropReason) -> bool {
    let outside = matches!(
        reason,
        DropReason::OutsideWindow | DropReason::UnknownTransfer
    );
    match id {
        None => outside,
        Some(id) => !outside && id.transfer_number() == transfer_number,
    }
}

// No events, for comparing with what a call returned: a bare `vec![]`
// could be a `Vec` of either event type.
pub fn none() -> Vec<Event> {
    Vec::new()
}

pub fn received(data: &'static [u8]) -> Event {
    received_with(Bytes::from_static(data), vec![])
}

pub fn received_with(data: Bytes, hints: Vec<HintItem>) -> Event {
    Event::Received {
        data,
        hints: Hints::from(hints),
    }
}

pub fn dropped(transfer_number: u32, reason: DropReason) -> Event {
    Event::MessageDropped {
        transfer_number,
        reason,
    }
}

pub fn rejected(transfer_number: u32, reason: RejectReason) -> Event {
    Event::TransferRejected {
        transfer_number,
        reason,
    }
}

pub fn expired(transfer_number: u32) -> Event {
    Event::TransferExpired { transfer_number }
}

pub fn cancelled(transfer_number: u32) -> Event {
    Event::TransferCancelled { transfer_number }
}

pub fn bundle_msg(data: &'static [u8]) -> Message {
    bundle_with(vec![], Bytes::from_static(data))
}

pub fn bundle_with(hints: Vec<HintItem>, data: Bytes) -> Message {
    Message::Bundle { hints, data }
}

pub fn segment(transfer_number: u32, segment_index: u32, data: &'static [u8]) -> Message {
    segment_with(
        transfer_number,
        segment_index,
        vec![],
        Bytes::from_static(data),
    )
}

pub fn segment_with(
    transfer_number: u32,
    segment_index: u32,
    hints: Vec<HintItem>,
    data: Bytes,
) -> Message {
    Message::TransferSegment(TransferSegmentMessage {
        transfer_number,
        segment_index,
        hints,
        data,
    })
}

pub fn end(transfer_number: u32, segment_index: u32, data: &'static [u8]) -> Message {
    end_with(
        transfer_number,
        segment_index,
        vec![],
        Bytes::from_static(data),
    )
}

pub fn end_with(
    transfer_number: u32,
    segment_index: u32,
    hints: Vec<HintItem>,
    data: Bytes,
) -> Message {
    Message::TransferEnd(TransferSegmentMessage {
        transfer_number,
        segment_index,
        hints,
        data,
    })
}

pub fn cancel(transfer_number: u32) -> Message {
    Message::TransferCancel { transfer_number }
}

// Encoded Transfer Cancels longer than the fixed four octets of Section
// 8.4: one with the H flag set and a one-byte unknown hint (type 0x40)
// before its Transfer Number, and one with two octets after it.
pub fn malformed_cancels(transfer_number: u32) -> [Bytes; 2] {
    let number = transfer_number.to_be_bytes();
    let hinted = [
        &[0x05, 0x80, 0x00, 0x07, 0x40 << 1, 0x01, 0xAA][..],
        &number,
    ]
    .concat();
    let long = [&[0x05, 0x00, 0x00, 0x06][..], &number, &[0xBB, 0xBB]].concat();
    [Bytes::from(hinted), Bytes::from(long)]
}

// A bundle of `len` filler bytes.
pub fn bundle(len: usize) -> Bytes {
    Bytes::from(vec![0x42; len])
}

// A bundle whose bytes differ, so a misplaced or reordered byte would show.
pub fn patterned(len: usize) -> Bytes {
    (0..len).map(|i| i as u8).collect()
}

// A payload `frame_kind` classifies as a BPv7 bundle: the CBOR
// indefinite-array header (0x9F) followed by filler.
pub fn bpv7_like(len: usize) -> Bytes {
    let mut v = vec![0x9F];
    v.resize(len, 0xAB);
    Bytes::from(v)
}

// Whether `inner` is a view into the allocation `outer` was decoded from.
pub fn is_within(inner: &Bytes, outer: &Bytes) -> bool {
    let start = outer.as_ptr() as usize;
    let p = inner.as_ptr() as usize;
    p >= start && p + inner.len() <= start + outer.len()
}

// One message on the wire, as a PDU of its own or a piece of one.
pub fn encode(msg: &Message) -> Bytes {
    let mut buf = BytesMut::new();
    encode_message(msg, &mut buf).unwrap();
    buf.freeze()
}

// A deterministic RNG for the `from_rng` constructors.  Implemented
// against the `rand` crate's `rand_core` re-export, proving this crate's
// `rand_core` version lines up with the `rand` in use.
#[cfg(feature = "rand")]
pub struct FixedRng(pub u32);

#[cfg(feature = "rand")]
impl TryRng for FixedRng {
    type Error = Infallible;
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

// An RNG that always fails, for the error path of the `try_from_rng`
// constructors.
#[cfg(feature = "rand")]
pub struct FailingRng;

#[cfg(feature = "rand")]
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("RNG failure")]
pub struct RngFailure;

#[cfg(feature = "rand")]
impl TryRng for FailingRng {
    type Error = RngFailure;
    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Err(RngFailure)
    }
    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Err(RngFailure)
    }
    fn try_fill_bytes(&mut self, _dst: &mut [u8]) -> Result<(), Self::Error> {
        Err(RngFailure)
    }
}
