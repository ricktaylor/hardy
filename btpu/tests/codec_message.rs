//! Message type, flag, and frame classification through the public `codec::message` API.

use hardy_btpu::codec::{
    Error,
    message::{
        FrameKind, MessageFlags, MessageType, frame_kind, is_reserved_bpv6, is_reserved_bpv7,
    },
};

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
