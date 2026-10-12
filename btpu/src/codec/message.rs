//! The messages of Section 8 and of the FEC extension, as the codec decodes
//! and encodes them.

use alloc::vec::Vec;

use bytes::Bytes;

use crate::{
    codec::{header::HEADER_SIZE, hint::HintItem},
    fec::{ExplicitFecMessage, PreAgreedFecMessage},
};

/// Size of the transfer number field, the whole content of a Transfer
/// Cancel and the start of every other transfer and FEC message's fields.
pub(crate) const TRANSFER_NUMBER_SIZE: usize = 4;

/// Size of a Transfer Segment or End message's fields after its hints: the
/// transfer number and the segment index.
pub(crate) const SEGMENT_FIELDS_SIZE: usize = TRANSFER_NUMBER_SIZE + 4;

/// Size of an FEC message's fields after its hints: the transfer number and
/// the instance or encoding ID byte.
pub(crate) const FEC_FIELDS_SIZE: usize = TRANSFER_NUMBER_SIZE + 1;

/// What a Transfer Segment or End message spends before its data, hints
/// aside: the header and [`SEGMENT_FIELDS_SIZE`].
pub(crate) const SEGMENT_FRAMING: usize = HEADER_SIZE + SEGMENT_FIELDS_SIZE;

/// BTP-U message type, as encoded in the first byte of the message header.
///
/// Only the type values this crate implements are representable; every
/// other value, reserved or unassigned, decodes as
/// [`Message::Unknown`] (see [`MessageType::from_byte`]).
///
/// The four FEC extension types have no IANA-assigned values yet: the FEC
/// draft lists them as TBD1..TBD4.  This crate uses 0x70..=0x73, from the
/// Private Use range of the BTPU Message Types registry (Section 12.1), as
/// provisional values.  They will change when the codes are assigned, and
/// peers must agree on them out of band until then.  Because the range is
/// Private Use, a decoder interprets these four values only when
/// [`DecodeOptions::fec`](crate::codec::DecodeOptions::fec) is set; a
/// deployment with its own private types is otherwise unaffected.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MessageType {
    /// Indefinite Padding Message (Section 8.6).  Special format: a single
    /// zero byte with no length field, so it never has a header of its own.
    /// Kept as the registry value: the decoder skips zero bytes and yields
    /// no [`Message`] for them.
    IndefinitePadding = 0x00,
    /// Definite Padding Message (Section 8.5).
    DefinitePadding = 0x01,
    /// Bundle Message (Section 8.1).  Complete bundle in a single message.
    Bundle = 0x02,
    /// Transfer Segment Message (Section 8.2).
    TransferSegment = 0x03,
    /// Transfer End Message (Section 8.3).
    TransferEnd = 0x04,
    /// Transfer Cancel Message (Section 8.4).
    TransferCancel = 0x05,
    /// Pre-agreed FEC Source Message (Section 4.1 of btpu-fec, TBD1;
    /// provisional value).
    PreAgreedFecSource = 0x70,
    /// Explicit FEC Source Message (Section 4.2 of btpu-fec, TBD2;
    /// provisional value).
    ExplicitFecSource = 0x71,
    /// Pre-agreed FEC Repair Message (Section 4.3 of btpu-fec, TBD3;
    /// provisional value).
    PreAgreedFecRepair = 0x72,
    /// Explicit FEC Repair Message (Section 4.4 of btpu-fec, TBD4;
    /// provisional value).
    ExplicitFecRepair = 0x73,
}

impl MessageType {
    /// The message type for a wire type byte, or `None` if this crate does
    /// not implement it.
    ///
    /// `None` is the normal outcome for every unassigned, Private Use, or
    /// bundle-reserved value; the caller decides what that means (the
    /// decoder relays such messages as [`Message::Unknown`]).
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::IndefinitePadding,
            0x01 => Self::DefinitePadding,
            0x02 => Self::Bundle,
            0x03 => Self::TransferSegment,
            0x04 => Self::TransferEnd,
            0x05 => Self::TransferCancel,
            0x70 => Self::PreAgreedFecSource,
            0x71 => Self::ExplicitFecSource,
            0x72 => Self::PreAgreedFecRepair,
            0x73 => Self::ExplicitFecRepair,
            _ => return None,
        })
    }

    /// Whether this is one of the four FEC extension message types.
    pub(crate) fn is_fec(self) -> bool {
        matches!(
            self,
            Self::PreAgreedFecSource
                | Self::ExplicitFecSource
                | Self::PreAgreedFecRepair
                | Self::ExplicitFecRepair
        )
    }
}

impl From<MessageType> for u8 {
    fn from(t: MessageType) -> u8 {
        t as u8
    }
}

/// Returns `true` if the type byte is the BPv6 reserved value (0x06), the
/// initial octet of a BPv6 bundle (Section 12.1).
pub(crate) fn is_reserved_bpv6(message_type: u8) -> bool {
    message_type == 0x06
}

/// Returns `true` if the type byte falls in the BPv7 reserved range
/// (0x80..=0x9F), the possible initial octets of a BPv7 bundle's CBOR array
/// (Section 12.1).
pub(crate) fn is_reserved_bpv7(message_type: u8) -> bool {
    (0x80..=0x9F).contains(&message_type)
}

/// Classification of a received link-layer frame's first byte.
///
/// BTP-U deliberately avoids the byte values that begin BPv6 (`0x06`) and
/// BPv7 (`0x80..=0x9F`) bundles, so one byte tells a bare bundle frame from
/// a PDU: the decoder reads a bare frame this way, and the sender emits one
/// only for data that classifies as a bundle. See [`frame_kind`].
///
/// All other first-byte values, including unallocated ranges, classify as
/// [`FrameKind::BtpuPdu`]. Future BTP-U message types may be assigned to
/// those unallocated bytes; a current decoder parses them as
/// [`Message::Unknown`] for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FrameKind {
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
/// [`FrameKind::BtpuPdu`] otherwise (including the empty-frame case).  The
/// same two predicates, [`is_reserved_bpv6`] and [`is_reserved_bpv7`],
/// define the ranges.
pub(crate) fn frame_kind(frame: &[u8]) -> FrameKind {
    match frame.first() {
        Some(&b) if is_reserved_bpv6(b) => FrameKind::Bpv6Bundle,
        Some(&b) if is_reserved_bpv7(b) => FrameKind::Bpv7Bundle,
        _ => FrameKind::BtpuPdu,
    }
}

/// The 4-bit message flags field (Section 7.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageFlags {
    /// Bit 0 (MSB): when set, Hint Items follow the header.
    pub hint: bool,
    /// The three currently-unassigned bits (the low 3 bits of the nibble),
    /// preserved verbatim.  The flags registry is Standards Action (Section
    /// 12.3), so future documents may assign them; zeroing them here would
    /// corrupt a relayed [`Message::Unknown`] from such a sender.  Locally
    /// originated messages leave this zero (Section 7.1: the sender MUST).
    pub rfu: u8,
}

impl MessageFlags {
    /// Encode into the 4-bit nibble (bits 7..4 of the second header byte).
    pub(crate) fn to_nibble(self) -> u8 {
        (if self.hint { 0x8 } else { 0 }) | (self.rfu & 0x7)
    }

    /// Decode from the 4-bit nibble.
    pub(crate) fn from_nibble(nibble: u8) -> Self {
        Self {
            hint: nibble & 0x8 != 0,
            rfu: nibble & 0x7,
        }
    }
}

/// A decoded BTP-U message.
///
/// There is no variant for Indefinite Padding: it is a run of zero bytes
/// with no header, which the decoder consumes silently and
/// [`pad_pdu`](crate::codec::pad_pdu) writes directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Definite Padding (type 1). Content is ignored.
    DefinitePadding {
        /// The declared content length; the content itself is never read.
        len: usize,
    },

    /// Complete bundle (type 2), or an encapsulated bundle in its native
    /// format found by the decoder (Section 7.3).
    ///
    /// The two are not told apart after decoding, and re-encoding always
    /// frames the bundle as a type 2 message: an encapsulated bundle does
    /// not relay byte-exact, and one longer than
    /// [`MAX_CONTENT_LENGTH`](crate::codec::header::MAX_CONTENT_LENGTH) is
    /// refused with [`EncodeError::LengthOverflow`](crate::codec::EncodeError::LengthOverflow).
    Bundle {
        /// Hint items carried by the message (always empty for an
        /// encapsulated bundle).
        hints: Vec<HintItem>,
        /// The bundle bytes.
        data: Bytes,
    },

    /// Transfer Segment (type 3, Section 8.2).
    TransferSegment(TransferSegmentMessage),

    /// Transfer End (type 4, Section 8.3): the transfer's final segment,
    /// with the same content layout as a Transfer Segment.
    TransferEnd(TransferSegmentMessage),

    /// Transfer Cancel (type 5).
    ///
    /// The decoder ignores any hints and any content after the Transfer
    /// Number: Section 8.4 gives the message a fixed length of 4 octets but
    /// does not say what a receiver does with any other length, and the
    /// header length frames the message so the extra octets can be
    /// skipped.  The Cancel is still honoured, since it states the
    /// sender's intent and is as trustworthy as the link that carried it.
    /// The encoder writes neither.
    TransferCancel {
        /// The transfer to abandon (Section 8.4).
        transfer_number: u32,
    },

    /// Pre-agreed FEC Source (provisional type 0x70, see [`fec`](crate::fec)):
    /// the payload carries an ADU.
    PreAgreedFecSource(PreAgreedFecMessage),

    /// Explicit FEC Source (provisional type 0x71, see [`fec`](crate::fec)):
    /// the payload carries an ADU.
    ExplicitFecSource(ExplicitFecMessage),

    /// Pre-agreed FEC Repair (provisional type 0x72, see [`fec`](crate::fec)):
    /// the payload carries repair symbols.
    PreAgreedFecRepair(PreAgreedFecMessage),

    /// Explicit FEC Repair (provisional type 0x73, see [`fec`](crate::fec)):
    /// the payload carries repair symbols.
    ExplicitFecRepair(ExplicitFecMessage),

    /// A message whose type this decoder does not interpret (an unassigned
    /// value, a Private Use value, or an FEC value with FEC decoding off).
    /// Preserved for forward compatibility.
    ///
    /// `data` is the raw, uninterpreted message content (any hint bytes
    /// included) and `flags` preserves the decoded flags, so re-encoding
    /// relays the message intact; in particular the H flag still frames
    /// the hint bytes sitting at the front of `data`.
    Unknown {
        /// The wire type byte.
        message_type: u8,
        /// The flags nibble, verbatim.
        flags: MessageFlags,
        /// The message content, verbatim.
        data: Bytes,
    },
}

impl Message {
    /// The wire type byte the message is encoded with.
    pub(crate) fn type_byte(&self) -> u8 {
        let mt = match self {
            Message::DefinitePadding { .. } => MessageType::DefinitePadding,
            Message::Bundle { .. } => MessageType::Bundle,
            Message::TransferSegment(_) => MessageType::TransferSegment,
            Message::TransferEnd(_) => MessageType::TransferEnd,
            Message::TransferCancel { .. } => MessageType::TransferCancel,
            Message::PreAgreedFecSource(_) => MessageType::PreAgreedFecSource,
            Message::ExplicitFecSource(_) => MessageType::ExplicitFecSource,
            Message::PreAgreedFecRepair(_) => MessageType::PreAgreedFecRepair,
            Message::ExplicitFecRepair(_) => MessageType::ExplicitFecRepair,
            Message::Unknown { message_type, .. } => return *message_type,
        };
        mt.into()
    }
}

/// Content of a Transfer Segment message (type 3, Section 8.2) or a
/// Transfer End message (type 4, Section 8.3).  The two have the same
/// layout: a Transfer End is the transfer's final segment, and only the
/// message type says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSegmentMessage {
    /// The transfer this segment belongs to.
    pub transfer_number: u32,
    /// The segment's position in the transfer, counted from zero.  In a
    /// Transfer End this is the final index `N`, and the transfer is
    /// complete once segments `0..=N` have all arrived.
    pub segment_index: u32,
    /// Hint items carried by the message.
    pub hints: Vec<HintItem>,
    /// The segment's bytes.
    pub data: Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_flags_nibble_round_trips_all_bits() {
        // The registry governs the whole nibble (Section 12.3, Standards
        // Action); every value must survive decode -> encode so relayed
        // unknown messages keep future-assigned bits.
        for nibble in 0..=0xF {
            let flags = MessageFlags::from_nibble(nibble);
            assert_eq!(flags.to_nibble(), nibble, "nibble {nibble:#x}");
            assert_eq!(flags.hint, nibble & 0x8 != 0, "nibble {nibble:#x}");
            assert_eq!(flags.rfu, nibble & 0x7, "nibble {nibble:#x}");
        }
    }

    #[test]
    fn from_byte_accepts_known_types() {
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
            assert_eq!(MessageType::from_byte(byte), Some(expected));
            assert_eq!(u8::from(expected), byte);
        }
    }

    #[test]
    fn from_byte_rejects_reserved_and_unassigned_values() {
        assert_eq!(MessageType::from_byte(0x06), None);
        for b in 0x80u8..=0x9F {
            assert_eq!(MessageType::from_byte(b), None, "byte {b:#04x}");
        }
        for b in [0x07u8, 0x10, 0x50, 0x6F, 0x74, 0xA0, 0xFF] {
            assert_eq!(MessageType::from_byte(b), None, "byte {b:#04x}");
        }
    }

    #[test]
    fn is_fec_covers_exactly_the_four_extension_types() {
        for t in [
            MessageType::PreAgreedFecSource,
            MessageType::ExplicitFecSource,
            MessageType::PreAgreedFecRepair,
            MessageType::ExplicitFecRepair,
        ] {
            assert!(t.is_fec(), "{t:?}");
        }
        for t in [
            MessageType::IndefinitePadding,
            MessageType::DefinitePadding,
            MessageType::Bundle,
            MessageType::TransferSegment,
            MessageType::TransferEnd,
            MessageType::TransferCancel,
        ] {
            assert!(!t.is_fec(), "{t:?}");
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
