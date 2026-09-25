use alloc::vec::Vec;

use bytes::Bytes;

use crate::{
    codec::hint::HintItem,
    fec::{ExplicitFecMessage, PreAgreedFecMessage},
};

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
pub enum MessageType {
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
    pub fn from_byte(b: u8) -> Option<Self> {
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
    pub fn is_fec(self) -> bool {
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
pub fn is_reserved_bpv6(message_type: u8) -> bool {
    message_type == 0x06
}

/// Returns `true` if the type byte falls in the BPv7 reserved range
/// (0x80..=0x9F), the possible initial octets of a BPv7 bundle's CBOR array
/// (Section 12.1).
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
/// All other first-byte values, including unallocated ranges, classify as
/// [`FrameKind::BtpuPdu`]. Future BTP-U message types may be assigned to
/// those unallocated bytes; a current decoder parses them as
/// [`Message::Unknown`] for forward compatibility.
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
/// [`FrameKind::BtpuPdu`] otherwise (including the empty-frame case).  The
/// same two predicates, [`is_reserved_bpv6`] and [`is_reserved_bpv7`],
/// define the ranges.
pub fn frame_kind(frame: &[u8]) -> FrameKind {
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
    pub fn to_nibble(self) -> u8 {
        (if self.hint { 0x8 } else { 0 }) | (self.rfu & 0x7)
    }

    /// Decode from the 4-bit nibble.
    pub fn from_nibble(nibble: u8) -> Self {
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
    /// refused with [`Error::LengthOverflow`](crate::codec::Error::LengthOverflow).
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
    TransferCancel {
        /// The transfer to abandon (Section 8.4).
        transfer_number: u32,
    },

    /// Pre-agreed FEC Source (provisional type 0x70, see [`MessageType`]):
    /// the payload carries an ADU.
    PreAgreedFecSource(PreAgreedFecMessage),

    /// Explicit FEC Source (provisional type 0x71, see [`MessageType`]):
    /// the payload carries an ADU.
    ExplicitFecSource(ExplicitFecMessage),

    /// Pre-agreed FEC Repair (provisional type 0x72, see [`MessageType`]):
    /// the payload carries repair symbols.
    PreAgreedFecRepair(PreAgreedFecMessage),

    /// Explicit FEC Repair (provisional type 0x73, see [`MessageType`]):
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
