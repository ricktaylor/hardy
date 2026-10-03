//! Fixtures shared between the integration-test binaries.

// Each test binary uses its own subset of these helpers, so per-binary
// unused ones are expected.
#![allow(dead_code)]

#[cfg(feature = "rand")]
use core::convert::Infallible;

use bytes::{Bytes, BytesMut};
use hardy_btpu::{
    codec::{
        encode_message,
        hint::{HintItem, HintType, HintValue, Hints},
        message::{Message, TransferSegmentMessage},
    },
    receiver::{MaxSegments, MaxTransferSize, Receiver, ReceiverConfig, ReceiverEvent},
    sender::{
        Carried, Error, LinkFraming, PduSize, SendId, SendOptions, SendQueueDepth, Sender,
        SenderConfig,
    },
    transfer::WindowSize,
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

// As `sender` with a 16-transfer window and the given link framing,
// admitting at most `depth` queue entries.
pub fn sender_with_queue_depth(pdu_size: usize, depth: usize, link_framing: LinkFraming) -> Sender {
    Sender::new(
        SenderConfig {
            send_queue_depth: SendQueueDepth::try_from(depth).unwrap(),
            link_framing,
            ..sender_config(pdu_size, 16)
        },
        0,
    )
}

// Enqueue `data` with no caller hints.
pub fn enqueue(s: &mut Sender, data: Bytes) -> Result<SendId, Error> {
    s.enqueue(data, SendOptions::default())
}

// Every PDU `s` has pending, in order.
pub fn drain(s: &mut Sender) -> Vec<Bytes> {
    let mut pdus = Vec::new();
    while let Some(pdu) = s.next_pdu() {
        pdus.push(pdu.data);
    }
    pdus
}

pub fn carried(id: SendId, completes: bool) -> Carried {
    Carried { id, completes }
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

pub fn received(data: &'static [u8]) -> ReceiverEvent {
    received_with(Bytes::from_static(data), vec![])
}

pub fn received_with(data: Bytes, hints: Vec<HintItem>) -> ReceiverEvent {
    ReceiverEvent::Received {
        data,
        hints: Hints::from(hints),
    }
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

// A bundle of `len` filler bytes.
pub fn bundle(len: usize) -> Bytes {
    Bytes::from(vec![0x42; len])
}

// A payload `frame_kind` classifies as a BPv7 bundle: the CBOR
// indefinite-array header (0x9F) followed by filler.
pub fn bpv7_like(len: usize) -> Bytes {
    let mut v = vec![0x9F];
    v.resize(len, 0xAB);
    Bytes::from(v)
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
