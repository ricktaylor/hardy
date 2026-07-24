use crate::codec::Error;
use crate::fec;
use crate::hint::HintItem;
use alloc::vec::Vec;
use bytes::Bytes;

/// BTP-U message type, as encoded in the first byte of the message header.
///
/// Only the type values defined by the BTP-U specification are representable.
/// Reserved values (0x06 and 0x80..=0x9F) and unrecognized values cause
/// [`TryFrom::try_from`] to return [`Error::InvalidMessageType`]; the codec
/// layer is responsible for distinguishing reserved-vs-unknown when needed.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MessageType {
    /// Indefinite Padding Message (Section 8.6). Special format: no length field.
    IndefinitePadding = 0x00,
    /// Definite Padding Message (Section 8.5).
    DefinitePadding = 0x01,
    /// Bundle Message (Section 8.1). Complete bundle in a single message.
    Bundle = 0x02,
    /// Transfer Segment Message (Section 8.2).
    TransferSegment = 0x03,
    /// Transfer End Message (Section 8.3).
    TransferEnd = 0x04,
    /// Transfer Cancel Message (Section 8.4).
    TransferCancel = 0x05,
    /// Pre-agreed FEC Source Message (Section 4.1 of btpu-fec).
    PreAgreedFecSource = 0x70,
    /// Explicit FEC Source Message (Section 4.2 of btpu-fec).
    ExplicitFecSource = 0x71,
    /// Pre-agreed FEC Repair Message (Section 4.3 of btpu-fec).
    PreAgreedFecRepair = 0x72,
    /// Explicit FEC Repair Message (Section 4.4 of btpu-fec).
    ExplicitFecRepair = 0x73,
}

impl From<MessageType> for u8 {
    fn from(t: MessageType) -> u8 {
        t as u8
    }
}

impl TryFrom<u8> for MessageType {
    type Error = Error;

    fn try_from(b: u8) -> Result<Self, Self::Error> {
        match b {
            0x00 => Ok(Self::IndefinitePadding),
            0x01 => Ok(Self::DefinitePadding),
            0x02 => Ok(Self::Bundle),
            0x03 => Ok(Self::TransferSegment),
            0x04 => Ok(Self::TransferEnd),
            0x05 => Ok(Self::TransferCancel),
            0x70 => Ok(Self::PreAgreedFecSource),
            0x71 => Ok(Self::ExplicitFecSource),
            0x72 => Ok(Self::PreAgreedFecRepair),
            0x73 => Ok(Self::ExplicitFecRepair),
            other => Err(Error::InvalidMessageType(other)),
        }
    }
}

/// Returns `true` if the type byte is the BPv6 reserved value (0x06), which
/// must not be used for BTP-U messages.
pub fn is_reserved_bpv6(message_type: u8) -> bool {
    message_type == 0x06
}

/// Returns `true` if the type byte falls in the BPv7 CBOR-array reserved
/// range (0x80..=0x9F), which must not be used for BTP-U messages.
pub fn is_reserved_bpv7(message_type: u8) -> bool {
    (0x80..=0x9F).contains(&message_type)
}

/// Classification of a received link-layer frame's first byte.
///
/// BTP-U deliberately avoids the byte values that begin BPv6 (`0x06`) and
/// BPv7 (`0x80..=0x9F`) bundles, so a CLA carrying a mix of BTP-U PDUs and
/// bare bundle frames on the same link can route each frame by inspecting a
/// single byte. See [`frame_kind`].
///
/// All other first-byte values — including unallocated ranges — classify as
/// [`FrameKind::BtpuPdu`]. Future BTP-U message types may be assigned to those
/// unallocated bytes; a current decoder parses them as [`Message::Unknown`]
/// for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameKind {
    /// A BTP-U PDU (or an empty frame, which decodes to zero messages).
    BtpuPdu,
    /// A BPv6 bundle (first byte = `0x06`).
    Bpv6Bundle,
    /// A BPv7 bundle (first byte in `0x80..=0x9F`, the CBOR array header range).
    Bpv7Bundle,
}

/// Classify a received frame by its first byte.
///
/// Returns [`FrameKind::Bpv6Bundle`] if the frame begins with `0x06`,
/// [`FrameKind::Bpv7Bundle`] if it begins with a byte in `0x80..=0x9F`, and
/// [`FrameKind::BtpuPdu`] otherwise (including the empty-frame case).
pub fn frame_kind(frame: &[u8]) -> FrameKind {
    match frame.first() {
        Some(&0x06) => FrameKind::Bpv6Bundle,
        Some(&b) if (0x80..=0x9F).contains(&b) => FrameKind::Bpv7Bundle,
        _ => FrameKind::BtpuPdu,
    }
}

/// The 4-bit message flags field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageFlags {
    /// Bit 0 (MSB): when set, Hint Items follow the header.
    pub hint: bool,
}

impl MessageFlags {
    /// Encode into the 4-bit nibble (bits 7..4 of the second header byte).
    pub fn to_nibble(self) -> u8 {
        if self.hint { 0x8 } else { 0 }
    }

    /// Decode from the 4-bit nibble.
    pub fn from_nibble(nibble: u8) -> Self {
        Self {
            hint: nibble & 0x8 != 0,
        }
    }
}

/// A decoded BTP-U message.
#[derive(Debug, Clone)]
pub enum Message {
    /// Indefinite Padding (type 0). Consumes zeros until a non-zero byte or
    /// end of PDU.
    IndefinitePadding,

    /// Definite Padding (type 1). Content is ignored.
    DefinitePadding(usize),

    /// Complete bundle (type 2).
    Bundle { hints: Vec<HintItem>, data: Bytes },

    /// Transfer Segment (type 3).
    TransferSegment(TransferSegmentMessage),

    /// Transfer End (type 4).
    TransferEnd(TransferEndMessage),

    /// Transfer Cancel (type 5).
    TransferCancel { transfer_number: u32 },

    /// Pre-agreed FEC Source (type 0x70).
    PreAgreedFecSource(fec::PreAgreedFecSourceMessage),

    /// Explicit FEC Source (type 0x71).
    ExplicitFecSource(fec::ExplicitFecSourceMessage),

    /// Pre-agreed FEC Repair (type 0x72).
    PreAgreedFecRepair(fec::PreAgreedFecRepairMessage),

    /// Explicit FEC Repair (type 0x73).
    ExplicitFecRepair(fec::ExplicitFecRepairMessage),

    /// An unrecognized message type. Preserved for forward compatibility.
    ///
    /// `data` is the raw, uninterpreted message content (any hint bytes
    /// included) and `flags` preserves the decoded flags, so re-encoding
    /// relays the message intact -- in particular the H flag still frames
    /// the hint bytes sitting at the front of `data`.
    Unknown {
        message_type: u8,
        flags: MessageFlags,
        data: Bytes,
    },
}

/// Content of a Transfer Segment message (type 3).
#[derive(Debug, Clone)]
pub struct TransferSegmentMessage {
    pub transfer_number: u32,
    pub segment_index: u32,
    pub hints: Vec<HintItem>,
    pub data: Bytes,
}

/// Content of a Transfer End message (type 4).
#[derive(Debug, Clone)]
pub struct TransferEndMessage {
    pub transfer_number: u32,
    pub segment_index: u32,
    pub hints: Vec<HintItem>,
    pub data: Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_accepts_known_types() {
        let cases = [
            (0x00, MessageType::IndefinitePadding),
            (0x01, MessageType::DefinitePadding),
            (0x02, MessageType::Bundle),
            (0x03, MessageType::TransferSegment),
            (0x04, MessageType::TransferEnd),
            (0x05, MessageType::TransferCancel),
            (0x70, MessageType::PreAgreedFecSource),
            (0x71, MessageType::ExplicitFecSource),
            (0x72, MessageType::PreAgreedFecRepair),
            (0x73, MessageType::ExplicitFecRepair),
        ];
        for (byte, expected) in cases {
            assert_eq!(MessageType::try_from(byte).unwrap(), expected);
        }
    }

    #[test]
    fn try_from_rejects_reserved_bpv6() {
        match MessageType::try_from(0x06) {
            Err(Error::InvalidMessageType(0x06)) => {}
            other => panic!("expected InvalidMessageType(0x06), got {other:?}"),
        }
    }

    #[test]
    fn try_from_rejects_reserved_bpv7_range() {
        for b in 0x80u8..=0x9F {
            assert!(
                matches!(MessageType::try_from(b), Err(Error::InvalidMessageType(x)) if x == b),
                "byte {b:#04x} should be rejected"
            );
        }
    }

    #[test]
    fn try_from_rejects_unknown_bytes() {
        for b in [0x07u8, 0x10, 0x50, 0x6F, 0x74, 0xA0, 0xFF] {
            assert!(
                matches!(MessageType::try_from(b), Err(Error::InvalidMessageType(x)) if x == b),
                "byte {b:#04x} should be rejected"
            );
        }
    }

    #[test]
    fn into_u8_round_trips() {
        for variant in [
            MessageType::IndefinitePadding,
            MessageType::DefinitePadding,
            MessageType::Bundle,
            MessageType::TransferSegment,
            MessageType::TransferEnd,
            MessageType::TransferCancel,
            MessageType::PreAgreedFecSource,
            MessageType::ExplicitFecSource,
            MessageType::PreAgreedFecRepair,
            MessageType::ExplicitFecRepair,
        ] {
            let byte: u8 = variant.into();
            assert_eq!(MessageType::try_from(byte).unwrap(), variant);
        }
    }

    #[test]
    fn is_reserved_bpv6_covers_value() {
        assert!(is_reserved_bpv6(0x06));
        assert!(!is_reserved_bpv6(0x05));
        assert!(!is_reserved_bpv6(0x07));
        assert!(!is_reserved_bpv6(0x00));
    }

    #[test]
    fn is_reserved_bpv7_covers_range() {
        assert!(!is_reserved_bpv7(0x7F));
        for b in 0x80u8..=0x9F {
            assert!(is_reserved_bpv7(b));
        }
        assert!(!is_reserved_bpv7(0xA0));
    }

    #[test]
    fn frame_kind_empty_is_btpu() {
        assert_eq!(frame_kind(&[]), FrameKind::BtpuPdu);
    }

    #[test]
    fn frame_kind_bpv6() {
        assert_eq!(frame_kind(&[0x06]), FrameKind::Bpv6Bundle);
        assert_eq!(frame_kind(&[0x06, 0x01, 0x02]), FrameKind::Bpv6Bundle);
    }

    #[test]
    fn frame_kind_bpv7_full_range() {
        for b in 0x80u8..=0x9F {
            assert_eq!(frame_kind(&[b]), FrameKind::Bpv7Bundle, "byte {b:#04x}");
        }
    }

    #[test]
    fn frame_kind_known_btpu_types_classify_as_pdu() {
        for b in [0x00u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x70, 0x71, 0x72, 0x73] {
            assert_eq!(frame_kind(&[b]), FrameKind::BtpuPdu, "byte {b:#04x}");
        }
    }

    #[test]
    fn frame_kind_unallocated_btpu_space_classifies_as_pdu() {
        // Bytes outside the reserved ranges and not yet assigned a BTP-U
        // message type still belong to BTP-U; decoders parse them as
        // Message::Unknown.
        for b in [0x07u8, 0x6F, 0x74, 0x7F, 0xA0, 0xFF] {
            assert_eq!(frame_kind(&[b]), FrameKind::BtpuPdu, "byte {b:#04x}");
        }
    }
}
