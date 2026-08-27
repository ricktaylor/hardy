//! Wire-format round trips and PDU framing through the public `codec` API.

use bytes::{BufMut, Bytes, BytesMut};
use hardy_btpu::{
    codec::{
        Error, Result, decode_pdu, encode_message, encoded_message_len,
        header::{HEADER_SIZE, MAX_CONTENT_LENGTH, MessageHeader, encode_header},
        hint::HintItem,
        message::{Message, MessageFlags, TransferEndMessage, TransferSegmentMessage},
        pad_pdu,
    },
    fec,
};

/// Collect the lazy decoder for tests that assert on a whole PDU.
fn decode_all(pdu: Bytes) -> Result<Vec<Message>> {
    decode_pdu(pdu).collect()
}

#[test]
fn round_trip_fec_messages() {
    // The payload is opaque: whatever FSSI/payload-ID/data bytes a scheme
    // packed into it must survive encode -> decode untouched.
    let payload = Bytes::from_static(b"\x01\x02fssi-or-id-plus-data");

    let messages = [
        Message::PreAgreedFecSource(fec::PreAgreedFecSourceMessage {
            transfer_number: 7,
            fec_instance_id: 3,
            hints: vec![],
            payload: payload.clone(),
        }),
        Message::ExplicitFecSource(fec::ExplicitFecSourceMessage {
            transfer_number: 7,
            fec_encoding_id: 3,
            hints: vec![],
            payload: payload.clone(),
        }),
        Message::PreAgreedFecRepair(fec::PreAgreedFecRepairMessage {
            transfer_number: 7,
            fec_instance_id: 3,
            hints: vec![],
            payload: payload.clone(),
        }),
        Message::ExplicitFecRepair(fec::ExplicitFecRepairMessage {
            transfer_number: 7,
            fec_encoding_id: 3,
            hints: vec![],
            payload: payload.clone(),
        }),
    ];

    for msg in &messages {
        let mut buf = BytesMut::new();
        encode_message(msg, &mut buf).unwrap();
        assert_eq!(buf.len(), encoded_message_len(msg));
        let decoded = decode_all(buf.freeze()).unwrap();
        assert_eq!(decoded.len(), 1);
        match (&decoded[0], msg) {
            (Message::PreAgreedFecSource(d), Message::PreAgreedFecSource(o)) => {
                assert_eq!(d.transfer_number, o.transfer_number);
                assert_eq!(d.fec_instance_id, o.fec_instance_id);
                assert_eq!(d.payload, o.payload);
            }
            (Message::ExplicitFecSource(d), Message::ExplicitFecSource(o)) => {
                assert_eq!(d.transfer_number, o.transfer_number);
                assert_eq!(d.fec_encoding_id, o.fec_encoding_id);
                assert_eq!(d.payload, o.payload);
            }
            (Message::PreAgreedFecRepair(d), Message::PreAgreedFecRepair(o)) => {
                assert_eq!(d.transfer_number, o.transfer_number);
                assert_eq!(d.fec_instance_id, o.fec_instance_id);
                assert_eq!(d.payload, o.payload);
            }
            (Message::ExplicitFecRepair(d), Message::ExplicitFecRepair(o)) => {
                assert_eq!(d.transfer_number, o.transfer_number);
                assert_eq!(d.fec_encoding_id, o.fec_encoding_id);
                assert_eq!(d.payload, o.payload);
            }
            (decoded, original) => {
                panic!("decoded {decoded:?} does not match original {original:?}")
            }
        }
    }
}

#[test]
fn round_trip_bundle_message() {
    let msg = Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"hello bundle"),
    };
    let mut buf = BytesMut::new();
    encode_message(&msg, &mut buf).unwrap();
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::Bundle { hints, data } => {
            assert!(hints.is_empty());
            assert_eq!(data.as_ref(), b"hello bundle");
        }
        other => panic!("Expected Bundle, got {other:?}"),
    }
}

#[test]
fn round_trip_bundle_with_hints() {
    let msg = Message::Bundle {
        hints: vec![HintItem::BundleLength(42)],
        data: Bytes::from_static(b"data"),
    };
    let mut buf = BytesMut::new();
    encode_message(&msg, &mut buf).unwrap();
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::Bundle { hints, data } => {
            assert_eq!(hints.len(), 1);
            assert_eq!(hints[0], HintItem::BundleLength(42));
            assert_eq!(data.as_ref(), b"data");
        }
        other => panic!("Expected Bundle, got {other:?}"),
    }
}

#[test]
fn round_trip_transfer_segment() {
    let msg = Message::TransferSegment(TransferSegmentMessage {
        transfer_number: 0x12345678,
        segment_index: 0,
        hints: vec![],
        data: Bytes::from_static(b"seg0"),
    });
    let mut buf = BytesMut::new();
    encode_message(&msg, &mut buf).unwrap();
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::TransferSegment(m) => {
            assert_eq!(m.transfer_number, 0x12345678);
            assert_eq!(m.segment_index, 0);
            assert_eq!(m.data.as_ref(), b"seg0");
        }
        other => panic!("Expected TransferSegment, got {other:?}"),
    }
}

#[test]
fn round_trip_transfer_end() {
    let msg = Message::TransferEnd(TransferEndMessage {
        transfer_number: 99,
        segment_index: 3,
        hints: vec![HintItem::BundleLength(1000)],
        data: Bytes::from_static(b"final"),
    });
    let mut buf = BytesMut::new();
    encode_message(&msg, &mut buf).unwrap();
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::TransferEnd(m) => {
            assert_eq!(m.transfer_number, 99);
            assert_eq!(m.segment_index, 3);
            assert_eq!(m.hints, vec![HintItem::BundleLength(1000)]);
            assert_eq!(m.data.as_ref(), b"final");
        }
        other => panic!("Expected TransferEnd, got {other:?}"),
    }
}

#[test]
fn round_trip_transfer_cancel() {
    let msg = Message::TransferCancel {
        transfer_number: 42,
    };
    let mut buf = BytesMut::new();
    encode_message(&msg, &mut buf).unwrap();
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::TransferCancel { transfer_number } => {
            assert_eq!(*transfer_number, 42);
        }
        other => panic!("Expected TransferCancel, got {other:?}"),
    }
}

#[test]
fn round_trip_definite_padding() {
    let msg = Message::DefinitePadding { len: 10 };
    let mut buf = BytesMut::new();
    encode_message(&msg, &mut buf).unwrap();
    assert_eq!(buf.len(), HEADER_SIZE + 10);
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert_eq!(decoded.len(), 1);
    assert!(matches!(&decoded[0], Message::DefinitePadding { len: 10 }));
}

#[test]
fn indefinite_padding_skipped() {
    // PDU: 3 zero bytes, then a Bundle message
    let bundle = Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"x"),
    };
    let mut buf = BytesMut::new();
    buf.put_bytes(0, 3); // indefinite padding
    encode_message(&bundle, &mut buf).unwrap();
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert_eq!(decoded.len(), 1);
    assert!(matches!(&decoded[0], Message::Bundle { .. }));
}

#[test]
fn all_zeros_pdu() {
    let pdu = Bytes::from(vec![0u8; 64]);
    let decoded = decode_all(pdu).unwrap();
    assert!(decoded.is_empty());
}

#[test]
fn multiple_messages_in_pdu() {
    let msgs = [
        Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"a"),
        },
        Message::TransferCancel { transfer_number: 1 },
        Message::DefinitePadding { len: 2 },
    ];
    let mut buf = BytesMut::new();
    for m in &msgs {
        encode_message(m, &mut buf).unwrap();
    }
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert_eq!(decoded.len(), 3);
}

#[test]
fn pad_pdu_fills_to_target() {
    let mut buf = BytesMut::new();
    let msg = Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"hi"),
    };
    encode_message(&msg, &mut buf).unwrap();
    let pre_pad_len = buf.len();
    pad_pdu(&mut buf, 64);
    assert_eq!(buf.len(), 64);

    // Verify the bundle is still decodable
    let decoded = decode_all(buf.clone().freeze()).unwrap();
    assert!(!decoded.is_empty());
    match &decoded[0] {
        Message::Bundle { data, .. } => assert_eq!(data.as_ref(), b"hi"),
        other => panic!("Expected Bundle, got {other:?}"),
    }

    // Padding already sufficient -- no-op
    pad_pdu(&mut buf, pre_pad_len);
    assert_eq!(buf.len(), 64);
}

#[test]
fn pad_pdu_small_remainder() {
    let mut buf = BytesMut::new();
    // Fill so that only 2 bytes remain (less than HEADER_SIZE)
    buf.put_bytes(0xFF, 62);
    pad_pdu(&mut buf, 64);
    assert_eq!(buf.len(), 64);
    // Last 2 bytes should be zeros (indefinite padding)
    assert_eq!(buf[62], 0);
    assert_eq!(buf[63], 0);
}

#[test]
fn pad_pdu_beyond_max_content_length_chains_messages() {
    // The largest single Definite Padding message.
    const MAX_MESSAGE: usize = HEADER_SIZE + MAX_CONTENT_LENGTH;

    // Exactly one maximum-size message fits.
    let mut buf = BytesMut::new();
    pad_pdu(&mut buf, MAX_MESSAGE);
    assert_eq!(buf.len(), MAX_MESSAGE);
    let decoded = decode_all(buf.freeze()).unwrap();
    assert!(matches!(
        &decoded[..],
        [Message::DefinitePadding {
            len: MAX_CONTENT_LENGTH
        }]
    ));

    // One byte past a single message's reach: the 20-bit length field
    // cannot declare it, so a second (indefinite) padding run follows —
    // the header must stay truthful rather than truncating.
    let mut buf = BytesMut::new();
    pad_pdu(&mut buf, MAX_MESSAGE + 1);
    assert_eq!(buf.len(), MAX_MESSAGE + 1);
    let decoded = decode_all(buf.freeze()).unwrap();
    assert!(matches!(
        &decoded[..],
        [Message::DefinitePadding {
            len: MAX_CONTENT_LENGTH
        }]
    ));

    // Enough space past the first message for a whole second header:
    // a chain of two Definite Padding messages.
    let mut buf = BytesMut::new();
    pad_pdu(&mut buf, MAX_MESSAGE + HEADER_SIZE + 1);
    assert_eq!(buf.len(), MAX_MESSAGE + HEADER_SIZE + 1);
    let decoded = decode_all(buf.freeze()).unwrap();
    assert!(matches!(
        &decoded[..],
        [
            Message::DefinitePadding {
                len: MAX_CONTENT_LENGTH
            },
            Message::DefinitePadding { len: 1 },
        ]
    ));
}

#[test]
fn bare_bpv6_bundle_decoded_as_bundle_message() {
    // A frame starting with the BPv6 reserved byte is treated as a bare
    // bundle and returned as a single Message::Bundle containing the
    // whole frame verbatim.
    let frame = Bytes::from_static(&[0x06, 0xDE, 0xAD, 0xBE, 0xEF]);
    let decoded = decode_all(frame.clone()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::Bundle { hints, data } => {
            assert!(hints.is_empty());
            assert_eq!(data.as_ref(), frame.as_ref());
        }
        other => panic!("Expected Bundle, got {other:?}"),
    }
}

#[test]
fn bare_bpv7_bundle_decoded_as_bundle_message() {
    // Any first byte in 0x80..=0x9F (CBOR array headers, how BPv7
    // bundles start) is treated as a bare bundle.
    for t in 0x80u8..=0x9F {
        let frame = Bytes::copy_from_slice(&[t, 0xCA, 0xFE, 0xBA, 0xBE]);
        let decoded = decode_all(frame.clone()).unwrap();
        assert_eq!(decoded.len(), 1, "byte {t:#04x}");
        match &decoded[0] {
            Message::Bundle { hints, data } => {
                assert!(hints.is_empty(), "byte {t:#04x}");
                assert_eq!(data.as_ref(), frame.as_ref(), "byte {t:#04x}");
            }
            other => panic!("byte {t:#04x}: Expected Bundle, got {other:?}"),
        }
    }
}

#[test]
fn mid_pdu_encapsulated_bundle_stops_iteration_keeping_prefix() {
    // A reserved byte mid-PDU marks raw bundle bytes (Section 12.1),
    // whose extent cannot be determined without parsing the bundle,
    // which this decoder does not implement.  Iteration must stop there
    // while keeping every message already parsed.
    let mut buf = BytesMut::new();
    // First message: a TransferCancel (8 bytes total: 4-byte header + 4-byte transfer_number)
    encode_message(&Message::TransferCancel { transfer_number: 1 }, &mut buf).unwrap();
    // Then append a header whose message_type byte is 0x06 (reserved).
    let mut hdr = [0u8; HEADER_SIZE];
    encode_header(
        &MessageHeader {
            message_type: 0x06,
            flags: MessageFlags::default(),
            length: 0,
        },
        &mut hdr,
    );
    buf.extend_from_slice(&hdr);
    let mut iter = decode_pdu(buf.freeze());
    assert!(matches!(
        iter.next(),
        Some(Ok(Message::TransferCancel { transfer_number: 1 }))
    ));
    assert!(matches!(
        iter.next(),
        Some(Err(Error::ReservedMessageType(0x06)))
    ));
    // A framing error exhausts the iterator: the reserved byte starts
    // raw bundle bytes with no BTP-U header, so the stream position is
    // unreliable and nothing further may be parsed.
    assert!(iter.is_exhausted());
    assert!(iter.next().is_none());
}

#[test]
fn malformed_interior_skips_only_that_message() {
    // A known-type message with a bounded extent but a malformed interior
    // (a hint header promising a 255-byte value with nothing behind it)
    // yields an Err, and iteration resumes at the next message boundary
    // given by the Section 7 header length.
    let mut pdu = BytesMut::new();
    pdu.put_u8(0x02); // Bundle
    pdu.put_u8(0x80); // H flag set, length high nibble 0
    pdu.put_u16(2);
    pdu.put_slice(b"\x1F\xFF"); // malformed hint chain
    encode_message(
        &Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"ok"),
        },
        &mut pdu,
    )
    .unwrap();

    let mut iter = decode_pdu(pdu.freeze());
    assert!(matches!(
        iter.next(),
        Some(Err(Error::InsufficientData { .. }))
    ));
    assert!(!iter.is_exhausted());
    assert!(matches!(
        iter.next(),
        Some(Ok(Message::Bundle { data, .. })) if data.as_ref() == b"ok"
    ));
    assert!(iter.next().is_none());
}

#[test]
fn length_past_buffer_is_terminal() {
    // A header promising more content than the PDU holds: the next
    // message boundary is unknowable, so iteration stops permanently
    // and the remainder is discarded.
    let mut pdu = BytesMut::new();
    pdu.put_u8(0x02); // Bundle
    pdu.put_u8(0x00);
    pdu.put_u16(100); // length 100, but no content follows
    let mut iter = decode_pdu(pdu.freeze());
    assert!(matches!(
        iter.next(),
        Some(Err(Error::InsufficientData { .. }))
    ));
    assert!(iter.is_exhausted());
    assert!(iter.next().is_none());
}

#[test]
fn unknown_type_preserved() {
    let mut pdu = BytesMut::new();
    let msg = Message::Unknown {
        message_type: 0x50,
        flags: MessageFlags::default(),
        data: Bytes::from_static(b"\x01\x02\x03"),
    };
    encode_message(&msg, &mut pdu).unwrap();
    let decoded = decode_all(pdu.freeze()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::Unknown {
            message_type,
            flags,
            data,
        } => {
            assert_eq!(*message_type, 0x50);
            assert!(!flags.hint);
            assert_eq!(data.as_ref(), b"\x01\x02\x03");
        }
        other => panic!("Expected Unknown, got {other:?}"),
    }
}

#[test]
fn unknown_message_with_hints_relays_intact() {
    // An unknown message with the H flag set must round-trip with the
    // flag AND the raw hint bytes preserved, or a relayed copy would be
    // misparsed downstream (hint bytes read as message body).
    let mut original = BytesMut::new();
    // header: type 0x50, H flag, length 5
    original.put_u8(0x50);
    original.put_u8(0x80); // flags nibble H=1, top 4 bits of length = 0
    original.put_u16(5);
    // content: a valid single hint (Bundle Length 42, 1-byte value) + data
    original.put_slice(b"\x00\x01\x2A"); // hint chain
    original.put_slice(b"xy"); // opaque body
    let original = original.freeze();

    let decoded = decode_all(original.clone()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::Unknown {
            message_type,
            flags,
            data,
        } => {
            assert_eq!(*message_type, 0x50);
            assert!(flags.hint);
            // Content is opaque: hint bytes stay in data, unparsed.
            assert_eq!(data.as_ref(), b"\x00\x01\x2Axy");
        }
        other => panic!("Expected Unknown, got {other:?}"),
    }

    let mut reencoded = BytesMut::new();
    encode_message(&decoded[0], &mut reencoded).unwrap();
    assert_eq!(reencoded.freeze(), original);
}

#[test]
fn unknown_message_rfu_flag_bits_relay_intact() {
    // Flags nibble 0xD: H plus two of the unassigned bits.  The flags
    // registry is Standards Action (Section 12.3), so a future sender
    // may validly set them; a relayed unknown message must keep the
    // nibble bit-exact.
    let mut original = BytesMut::new();
    original.put_u8(0x50); // unknown type
    original.put_u8(0xD0); // flags nibble 0xD, length high nibble 0
    original.put_u16(2);
    original.put_slice(b"xy"); // opaque content
    let original = original.freeze();

    let decoded = decode_all(original.clone()).unwrap();
    assert_eq!(decoded.len(), 1);
    match &decoded[0] {
        Message::Unknown { flags, .. } => {
            assert!(flags.hint);
            assert_eq!(flags.rfu, 0x5);
        }
        other => panic!("Expected Unknown, got {other:?}"),
    }

    let mut reencoded = BytesMut::new();
    encode_message(&decoded[0], &mut reencoded).unwrap();
    assert_eq!(reencoded.freeze(), original);
}

#[test]
fn malformed_hints_in_unknown_message_do_not_poison_pdu() {
    // Unknown messages are skipped via the Section 7 header length
    // field.  A hint chain we cannot parse (here: invalid Bundle Length size)
    // inside an unknown message must not error the PDU; the following
    // Bundle message must still decode.
    let mut pdu = BytesMut::new();
    // Unknown message, H flag set, content = garbage "hints".
    pdu.put_u8(0x50);
    pdu.put_u8(0x80);
    pdu.put_u16(2);
    pdu.put_slice(b"\x1F\xFF"); // malformed hint chain
    // Followed by a well-formed Bundle message.
    encode_message(
        &Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"ok"),
        },
        &mut pdu,
    )
    .unwrap();

    let decoded = decode_all(pdu.freeze()).unwrap();
    assert_eq!(decoded.len(), 2);
    assert!(matches!(
        &decoded[0],
        Message::Unknown {
            message_type: 0x50,
            ..
        }
    ));
    assert!(matches!(
        &decoded[1],
        Message::Bundle { data, .. } if data.as_ref() == b"ok"
    ));
}

#[test]
fn encoded_message_len_accurate() {
    let messages = [
        Message::IndefinitePadding,
        Message::DefinitePadding { len: 10 },
        Message::Bundle {
            hints: vec![HintItem::BundleLength(500)],
            data: Bytes::from_static(b"test data"),
        },
        Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 1,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"segment"),
        }),
        Message::TransferCancel { transfer_number: 1 },
    ];
    for msg in &messages {
        let predicted = encoded_message_len(msg);
        let mut buf = BytesMut::new();
        encode_message(msg, &mut buf).unwrap();
        assert_eq!(
            buf.len(),
            predicted,
            "Length mismatch for {msg:?}: predicted {predicted}, actual {}",
            buf.len()
        );
    }
}
