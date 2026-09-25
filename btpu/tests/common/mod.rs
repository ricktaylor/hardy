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
        message::{Message, TransferSegmentMessage},
    },
    receiver::{MaxBundleSize, MaxSegments, Receiver, ReceiverConfig},
    sender::{PduSize, Sender, SenderConfig},
    transfer::WindowSize,
};
#[cfg(feature = "rand")]
use rand::rand_core::TryRng;

pub fn window_size(v: u16) -> WindowSize {
    WindowSize::try_from(v).unwrap()
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

// A receiver with the given window size and bundle-size cap and FEC
// decoding off.
pub fn receiver(window: u16, max_bundle_size: usize) -> Receiver {
    Receiver::new(ReceiverConfig {
        window_size: window_size(window),
        max_bundle_size: MaxBundleSize::try_from(max_bundle_size).unwrap(),
        max_segments_per_transfer: None,
        max_retained_bytes: None,
        fec: false,
    })
}

// As `receiver`, with a per-transfer segment limit.
pub fn receiver_with_max_segments(
    window: u16,
    max_bundle_size: usize,
    max_segments: MaxSegments,
) -> Receiver {
    Receiver::new(ReceiverConfig {
        window_size: window_size(window),
        max_bundle_size: MaxBundleSize::try_from(max_bundle_size).unwrap(),
        max_segments_per_transfer: Some(max_segments),
        max_retained_bytes: None,
        fec: false,
    })
}

pub fn segment(transfer_number: u32, segment_index: u32, data: &'static [u8]) -> Message {
    Message::TransferSegment(TransferSegmentMessage {
        transfer_number,
        segment_index,
        hints: vec![],
        data: Bytes::from_static(data),
    })
}

pub fn end(transfer_number: u32, segment_index: u32, data: &'static [u8]) -> Message {
    Message::TransferEnd(TransferSegmentMessage {
        transfer_number,
        segment_index,
        hints: vec![],
        data: Bytes::from_static(data),
    })
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
