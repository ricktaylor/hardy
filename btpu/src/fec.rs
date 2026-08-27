//! FEC extension message types (draft-ietf-dtn-btpu-fec).
//!
//! This module defines the content of the four message types introduced by
//! the BTP-U FEC extension.  The crate frames them (encode, decode, and
//! window tracking) but implements no FEC scheme and performs no FEC
//! reassembly; the payload of each message is carried opaquely.
//!
//! The four types share two layouts.  A pre-agreed message names a
//! scheme instance configured out of band; an explicit one names an FEC
//! Encoding ID and carries the scheme's inline parameters in its payload.
//! Whether the payload holds source data or repair symbols is decided by
//! the [`Message`](crate::codec::message::Message) variant that wraps the
//! content, not by the content itself.
//!
//! The message type codes are not yet IANA-assigned (the FEC draft lists
//! them as TBD1..TBD4); the values used here are provisional Private Use
//! codes, see [`MessageType`](crate::codec::message::MessageType).  Because
//! the range is Private Use, a decoder interprets them only when asked to
//! via [`DecodeOptions::fec`](crate::codec::DecodeOptions::fec) or
//! [`ReceiverConfig::fec`](crate::receiver::ReceiverConfig::fec); otherwise
//! they relay as [`Message::Unknown`](crate::codec::message::Message::Unknown).

use alloc::vec::Vec;

use bytes::Bytes;

use crate::codec::hint::HintItem;

/// Content of a Pre-agreed FEC Source or Repair message (provisional types
/// 0x70 and 0x72), for a transfer using a pre-configured FEC scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreAgreedFecMessage {
    /// The transfer this message belongs to.
    pub transfer_number: u32,
    /// Identifies the pre-agreed FEC scheme instance.
    pub fec_instance_id: u8,
    /// Hint items carried by the message.
    pub hints: Vec<HintItem>,
    /// The FEC Payload ID followed by the source ADU data (Source message)
    /// or the repair symbols (Repair message), as raw bytes.
    ///
    /// The boundary between the two is defined by the pre-agreed scheme and
    /// cannot be determined here.
    pub payload: Bytes,
}

/// Content of an Explicit FEC Source or Repair message (provisional types
/// 0x71 and 0x73), which carries its FEC-Scheme-Specific Information
/// (FSSI) inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitFecMessage {
    /// The transfer this message belongs to.
    pub transfer_number: u32,
    /// The FEC Encoding ID of the scheme (RFC 6363 Section 5.6).
    pub fec_encoding_id: u8,
    /// Hint items carried by the message.
    pub hints: Vec<HintItem>,
    /// The FSSI, then the FEC Payload ID, then the source ADU data (Source
    /// message) or the repair symbols (Repair message), as raw bytes.
    ///
    /// The boundaries are defined by the scheme identified by
    /// `fec_encoding_id` and cannot be determined here.
    pub payload: Bytes,
}
