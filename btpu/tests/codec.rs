//! Wire-format round trips and PDU framing through the public `codec` API.

mod common;

use bytes::{BufMut, Bytes, BytesMut};
use hardy_btpu::{
    codec::{
        DecodeOptions, Error, Result, decode_pdu, decode_pdu_with, encode_message,
        encoded_message_len,
        header::{HEADER_SIZE, MAX_CONTENT_LENGTH},
        hint::{BUNDLE_LENGTH_HINT, HintItem, ValidationError},
        message::{Message, MessageFlags, TransferSegmentMessage},
        pad_pdu,
    },
    fec::{ExplicitFecMessage, PreAgreedFecMessage},
};

use self::common::encode;

const FEC: DecodeOptions<'static> = DecodeOptions {
    fec: true,
    bundle_extent: None,
};

// Collect the lazy decoder for tests that assert on a whole PDU.
fn decode_all(pdu: Bytes) -> Result<Vec<Message>> {
    decode_pdu(pdu).collect()
}

fn decode_all_with(pdu: Bytes, options: DecodeOptions<'_>) -> Result<Vec<Message>> {
    decode_pdu_with(pdu, options).collect()
}

fn cancel(transfer_number: u32) -> Message {
    Message::TransferCancel { transfer_number }
}

fn fec_messages() -> [Message; 4] {
    let payload = Bytes::from_static(b"\x01\x02fssi-or-id-plus-data");
    [
        Message::PreAgreedFecSource(PreAgreedFecMessage {
            transfer_number: 7,
            fec_instance_id: 3,
            hints: vec![],
            payload: payload.clone(),
        }),
        Message::ExplicitFecSource(ExplicitFecMessage {
            transfer_number: 7,
            fec_encoding_id: 3,
            hints: vec![HintItem::BundleLength(9)],
            payload: payload.clone(),
        }),
        Message::PreAgreedFecRepair(PreAgreedFecMessage {
            transfer_number: 7,
            fec_instance_id: 3,
            hints: vec![],
            payload: payload.clone(),
        }),
        Message::ExplicitFecRepair(ExplicitFecMessage {
            transfer_number: 7,
            fec_encoding_id: 3,
            hints: vec![],
            payload,
        }),
    ]
}

#[test]
fn round_trip_fec_messages_with_fec_decoding_on() {
    // The payload is opaque: whatever FSSI/payload-ID/data bytes a scheme
    // packed into it must survive encode -> decode untouched.
    for msg in &fec_messages() {
        let wire = encode(msg);
        assert_eq!(wire.len(), encoded_message_len(msg));
        assert_eq!(decode_all_with(wire, FEC).unwrap(), vec![msg.clone()]);
    }
}

#[test]
fn fec_types_relay_as_unknown_by_default() {
    // 0x70..=0x73 are Private Use (Section 12.1): a decoder that was not
    // told to expect the FEC extension must treat them like any other
    // unknown type, byte-exact, so a peer's private types are untouched.
    for msg in &fec_messages() {
        let wire = encode(msg);
        let decoded = decode_all(wire.clone()).unwrap();
        let [
            Message::Unknown {
                message_type,
                flags,
                data,
            },
        ] = decoded.as_slice()
        else {
            panic!("expected one Unknown, got {decoded:?}");
        };
        assert!((0x70..=0x73).contains(message_type));
        assert_eq!(
            flags.hint,
            !matches!(
                msg,
                Message::PreAgreedFecSource(_)
                    | Message::PreAgreedFecRepair(_)
                    | Message::ExplicitFecRepair(_)
            )
        );
        assert_eq!(data.len(), wire.len() - HEADER_SIZE);
        assert_eq!(encode(&decoded[0]), wire);
    }
}

#[test]
fn round_trip_core_messages() {
    let messages = [
        Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"hello bundle"),
        },
        Message::Bundle {
            hints: vec![HintItem::BundleLength(42)],
            data: Bytes::from_static(b"data"),
        },
        Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 0x12345678,
            segment_index: 0,
            hints: vec![],
            data: Bytes::from_static(b"seg0"),
        }),
        Message::TransferEnd(TransferSegmentMessage {
            transfer_number: 99,
            segment_index: 3,
            hints: vec![HintItem::BundleLength(1000)],
            data: Bytes::from_static(b"final"),
        }),
        cancel(42),
        Message::DefinitePadding { len: 10 },
    ];
    for msg in &messages {
        let wire = encode(msg);
        assert_eq!(wire.len(), encoded_message_len(msg), "{msg:?}");
        assert_eq!(decode_all(wire).unwrap(), vec![msg.clone()]);
    }
}

#[test]
fn indefinite_padding_skipped() {
    let bundle = Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"x"),
    };
    let mut buf = BytesMut::new();
    buf.put_bytes(0, 3);
    buf.put_slice(&encode(&bundle));
    buf.put_bytes(0, 2);
    assert_eq!(decode_all(buf.freeze()).unwrap(), vec![bundle]);
}

#[test]
fn all_zeros_pdu() {
    assert_eq!(decode_all(Bytes::from(vec![0u8; 64])).unwrap(), vec![]);
}

#[test]
fn multiple_messages_in_pdu() {
    let msgs = vec![
        Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"a"),
        },
        cancel(1),
        Message::DefinitePadding { len: 2 },
    ];
    let mut buf = BytesMut::new();
    for m in &msgs {
        buf.put_slice(&encode(m));
    }
    assert_eq!(decode_all(buf.freeze()).unwrap(), msgs);
}

#[test]
fn pad_pdu_fills_to_target() {
    let msg = Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"hi"),
    };
    let mut buf = BytesMut::from(encode(&msg).as_ref());
    let pre_pad_len = buf.len();
    pad_pdu(&mut buf, 64);
    assert_eq!(buf.len(), 64);
    assert_eq!(
        decode_all(buf.clone().freeze()).unwrap(),
        vec![
            msg,
            Message::DefinitePadding {
                len: 64 - pre_pad_len - HEADER_SIZE
            }
        ]
    );

    // Padding already sufficient: a no-op.
    pad_pdu(&mut buf, pre_pad_len);
    assert_eq!(buf.len(), 64);
}

#[test]
fn pad_pdu_small_remainder() {
    let mut buf = BytesMut::new();
    // Fill so that only 2 bytes remain (less than HEADER_SIZE).
    buf.put_bytes(0xFF, 62);
    pad_pdu(&mut buf, 64);
    assert_eq!(&buf[62..], &[0, 0]);
}

#[test]
fn pad_pdu_beyond_max_content_length_chains_messages() {
    // The largest single Definite Padding message.
    const MAX_MESSAGE: usize = HEADER_SIZE + MAX_CONTENT_LENGTH;

    // Exactly one maximum-size message fits.
    let mut buf = BytesMut::new();
    pad_pdu(&mut buf, MAX_MESSAGE);
    assert_eq!(buf.len(), MAX_MESSAGE);
    assert_eq!(
        decode_all(buf.freeze()).unwrap(),
        vec![Message::DefinitePadding {
            len: MAX_CONTENT_LENGTH
        }]
    );

    // One byte past a single message's reach: the 20-bit length field
    // cannot declare it, so an indefinite padding byte follows; the header
    // must stay truthful rather than truncating.
    let mut buf = BytesMut::new();
    pad_pdu(&mut buf, MAX_MESSAGE + 1);
    assert_eq!(buf.len(), MAX_MESSAGE + 1);
    assert_eq!(
        decode_all(buf.freeze()).unwrap(),
        vec![Message::DefinitePadding {
            len: MAX_CONTENT_LENGTH
        }]
    );

    // Enough space past the first message for a whole second header:
    // a chain of two Definite Padding messages.
    let mut buf = BytesMut::new();
    pad_pdu(&mut buf, MAX_MESSAGE + HEADER_SIZE + 1);
    assert_eq!(buf.len(), MAX_MESSAGE + HEADER_SIZE + 1);
    assert_eq!(
        decode_all(buf.freeze()).unwrap(),
        vec![
            Message::DefinitePadding {
                len: MAX_CONTENT_LENGTH
            },
            Message::DefinitePadding { len: 1 },
        ]
    );
}

#[test]
fn bare_bpv6_bundle_decoded_as_bundle_message() {
    // Without an extent hook, a frame starting with the BPv6 reserved byte
    // is a bare bundle running to the end of the frame.
    let frame = Bytes::from_static(&[0x06, 0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(
        decode_all(frame.clone()).unwrap(),
        vec![Message::Bundle {
            hints: vec![],
            data: frame
        }]
    );
}

#[test]
fn bare_bpv7_bundle_decoded_as_bundle_message() {
    // Any first byte in 0x80..=0x9F (CBOR array headers, how BPv7 bundles
    // start) is treated as a bare bundle.
    for t in 0x80u8..=0x9F {
        let frame = Bytes::copy_from_slice(&[t, 0xCA, 0xFE, 0xBA, 0xBE]);
        assert_eq!(
            decode_all(frame.clone()).unwrap(),
            vec![Message::Bundle {
                hints: vec![],
                data: frame
            }],
            "byte {t:#04x}"
        );
    }
}

#[test]
fn bare_bundle_after_indefinite_padding_is_the_rest_of_the_frame() {
    // Leading zeros are Indefinite Padding; with no message yet framed the
    // hookless rule still applies and the bundle runs to the end.
    let frame = Bytes::from_static(&[0, 0, 0x9F, 1, 2]);
    assert_eq!(
        decode_all(frame.clone()).unwrap(),
        vec![Message::Bundle {
            hints: vec![],
            data: frame.slice(2..)
        }]
    );
}

#[test]
fn bare_frame_zero_fill_is_delivered_as_bundle_bytes_without_hook() {
    // The documented pitfall: a 4-byte bundle zero-filled to Ethernet's
    // 46-byte minimum is delivered as 46 bytes when nothing can delimit it.
    let mut frame = vec![0x9F, 1, 2, 0xFF];
    frame.resize(46, 0);
    let frame = Bytes::from(frame);
    assert_eq!(
        decode_all(frame.clone()).unwrap(),
        vec![Message::Bundle {
            hints: vec![],
            data: frame
        }]
    );
}

// A stand-in for a bundle parser: a 0x9F "bundle" here is four bytes long,
// or unparseable if fewer than four remain.
fn four_byte_bundles(bytes: &[u8]) -> Option<usize> {
    (bytes.first() == Some(&0x9F) && bytes.len() >= 4).then_some(4)
}

#[test]
fn extent_hook_trims_bare_frame_padding() {
    let mut frame = vec![0x9F, 1, 2, 0xFF];
    frame.resize(46, 0);
    let frame = Bytes::from(frame);
    let options = DecodeOptions {
        fec: false,
        bundle_extent: Some(&four_byte_bundles),
    };
    // The bundle is exactly its four bytes; the zero-fill behind it decodes
    // as Indefinite Padding.
    assert_eq!(
        decode_all_with(frame.clone(), options).unwrap(),
        vec![Message::Bundle {
            hints: vec![],
            data: frame.slice(..4)
        }]
    );
}

#[test]
fn extent_hook_delivers_mid_pdu_bundle_and_iteration_continues() {
    // Section 7.3: a receiver that can delimit an encapsulated bundle
    // handles it as a Bundle Message and continues with what follows.
    let mut pdu = BytesMut::new();
    pdu.put_slice(&encode(&cancel(1)));
    pdu.put_slice(&[0x9F, 1, 2, 3]);
    pdu.put_slice(&encode(&cancel(2)));
    let pdu = pdu.freeze();
    let options = DecodeOptions {
        fec: false,
        bundle_extent: Some(&four_byte_bundles),
    };
    assert_eq!(
        decode_all_with(pdu.clone(), options).unwrap(),
        vec![
            cancel(1),
            Message::Bundle {
                hints: vec![],
                data: pdu.slice(8..12)
            },
            cancel(2),
        ]
    );
}

#[test]
fn mid_pdu_encapsulated_bundle_without_hook_is_terminal() {
    // With nothing to delimit it, the bundle's extent is unknowable and
    // Section 7.3 forbids processing the remainder: iteration stops, and
    // every message already parsed is kept.  The error locates the bundle
    // in a clone of the PDU kept by the caller.
    let mut pdu = BytesMut::new();
    pdu.put_slice(&encode(&cancel(1)));
    let bundle_offset = pdu.len();
    pdu.put_slice(&[0x06, 0, 0, 0]);
    let pdu = pdu.freeze();
    let mut iter = decode_pdu(pdu.clone());
    assert_eq!(iter.next(), Some(Ok(cancel(1))));
    let Some(Err(Error::EncapsulatedBundle { first_byte, offset })) = iter.next() else {
        panic!("expected a terminal EncapsulatedBundle");
    };
    assert_eq!((first_byte, offset), (0x06, bundle_offset));
    assert_eq!(pdu[offset..], [0x06, 0, 0, 0]);
    assert!(iter.is_exhausted());
    assert_eq!(iter.next(), None);
}

#[test]
fn extent_hook_declining_is_terminal() {
    // The hook says it cannot delimit the bytes: same outcome as no hook,
    // and no guess is made even at the start of the PDU.
    let frame = Bytes::from_static(&[0x9F, 1]);
    let options = DecodeOptions {
        fec: false,
        bundle_extent: Some(&four_byte_bundles),
    };
    let mut iter = decode_pdu_with(frame, options);
    assert_eq!(
        iter.next(),
        Some(Err(Error::EncapsulatedBundle {
            first_byte: 0x9F,
            offset: 0
        }))
    );
    assert!(iter.is_exhausted());
    assert_eq!(iter.next(), None);
}

#[test]
fn extent_hook_claiming_zero_bytes_is_terminal() {
    // A zero extent would deliver an empty bundle and leave the decoder at
    // the same offset; it is treated as the hook declining.
    let frame = Bytes::from_static(&[0x9F, 1, 2, 3]);
    let zero = |_: &[u8]| Some(0);
    let options = DecodeOptions {
        fec: false,
        bundle_extent: Some(&zero),
    };
    let mut iter = decode_pdu_with(frame, options);
    assert_eq!(
        iter.next(),
        Some(Err(Error::EncapsulatedBundle {
            first_byte: 0x9F,
            offset: 0
        }))
    );
    assert!(iter.is_exhausted());
    assert_eq!(iter.next(), None);
}

#[test]
fn extent_hook_overrunning_the_pdu_is_terminal() {
    let frame = Bytes::from_static(&[0x9F, 1, 2]);
    let overrun = |_: &[u8]| Some(100);
    let options = DecodeOptions {
        fec: false,
        bundle_extent: Some(&overrun),
    };
    let mut iter = decode_pdu_with(frame, options);
    assert_eq!(
        iter.next(),
        Some(Err(Error::InsufficientData {
            needed: 100,
            available: 3,
        }))
    );
    assert!(iter.is_exhausted());
}

#[test]
fn malformed_interior_skips_only_that_message() {
    // A known-type message with a bounded extent but a malformed interior
    // (a hint header promising a 255-byte value with nothing behind it)
    // yields an Err, and iteration resumes at the next message boundary
    // given by the Section 7 header length (Section 7.3 skip-and-continue).
    let ok = Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"ok"),
    };
    let mut pdu = BytesMut::new();
    pdu.put_u8(0x02); // Bundle
    pdu.put_u8(0x80); // H flag set, length high nibble 0
    pdu.put_u16(2);
    pdu.put_slice(b"\x1F\xFF"); // malformed hint chain
    pdu.put_slice(&encode(&ok));

    let mut iter = decode_pdu(pdu.freeze());
    assert_eq!(
        iter.next(),
        Some(Err(Error::InsufficientData {
            needed: 257,
            available: 2,
        }))
    );
    assert!(!iter.is_exhausted());
    assert_eq!(iter.next(), Some(Ok(ok)));
    assert_eq!(iter.next(), None);
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
    assert_eq!(
        iter.next(),
        Some(Err(Error::InsufficientData {
            needed: 104,
            available: 4,
        }))
    );
    assert!(iter.is_exhausted());
    assert_eq!(iter.next(), None);
}

#[test]
fn short_bodies_are_contained_to_their_message() {
    // Each core message type with a body shorter than its fixed fields:
    // the header still bounds the message, so the fault is recoverable
    // and the Cancel behind it decodes.
    let cases: [(u8, usize, usize); 3] = [
        (0x05, 3, 4), // Cancel: 4-byte transfer number
        (0x03, 7, 8), // Segment: transfer number + segment index
        (0x04, 7, 8), // End: transfer number + segment index
    ];
    for (message_type, body_len, needed) in cases {
        let mut pdu = BytesMut::new();
        pdu.put_u8(message_type);
        pdu.put_u8(0);
        pdu.put_u16(body_len as u16);
        pdu.put_bytes(0xAA, body_len);
        pdu.put_slice(&encode(&cancel(9)));
        let mut iter = decode_pdu(pdu.freeze());
        assert_eq!(
            iter.next(),
            Some(Err(Error::InsufficientData {
                needed,
                available: body_len,
            })),
            "type {message_type:#04x}"
        );
        assert!(!iter.is_exhausted());
        assert_eq!(iter.next(), Some(Ok(cancel(9))));
    }

    // FEC messages: transfer number + one ID byte.
    for message_type in 0x70u8..=0x73 {
        let mut pdu = BytesMut::new();
        pdu.put_u8(message_type);
        pdu.put_u8(0);
        pdu.put_u16(4);
        pdu.put_bytes(0xAA, 4);
        pdu.put_slice(&encode(&cancel(9)));
        let mut iter = decode_pdu_with(pdu.freeze(), FEC);
        assert_eq!(
            iter.next(),
            Some(Err(Error::InsufficientData {
                needed: 5,
                available: 4,
            })),
            "type {message_type:#04x}"
        );
        assert!(!iter.is_exhausted());
        assert_eq!(iter.next(), Some(Ok(cancel(9))));
    }
}

#[test]
fn unknown_type_preserved() {
    let msg = Message::Unknown {
        message_type: 0x50,
        flags: MessageFlags::default(),
        data: Bytes::from_static(b"\x01\x02\x03"),
    };
    assert_eq!(decode_all(encode(&msg)).unwrap(), vec![msg]);
}

#[test]
fn unknown_message_with_hints_relays_intact() {
    // An unknown message with the H flag set must round-trip with the
    // flag AND the raw hint bytes preserved, or a relayed copy would be
    // misparsed downstream (hint bytes read as message body).
    let mut original = BytesMut::new();
    original.put_u8(0x50);
    original.put_u8(0x80); // flags nibble H=1, top 4 bits of length = 0
    original.put_u16(5);
    original.put_slice(b"\x00\x01\x2A"); // a valid hint chain
    original.put_slice(b"xy"); // opaque body
    let original = original.freeze();

    let decoded = decode_all(original.clone()).unwrap();
    assert_eq!(
        decoded,
        vec![Message::Unknown {
            message_type: 0x50,
            flags: MessageFlags { hint: true, rfu: 0 },
            data: original.slice(HEADER_SIZE..),
        }]
    );
    assert_eq!(encode(&decoded[0]), original);
}

#[test]
fn unknown_message_rfu_flag_bits_relay_intact() {
    // Flags nibble 0xD: H plus two of the unassigned bits.  The flags
    // registry is Standards Action (Section 12.3), so a future sender
    // may validly set them; a relayed unknown message must keep the
    // nibble bit-exact.
    let mut original = BytesMut::new();
    original.put_u8(0x50);
    original.put_u8(0xD0);
    original.put_u16(2);
    original.put_slice(b"xy");
    let original = original.freeze();

    let decoded = decode_all(original.clone()).unwrap();
    assert_eq!(
        decoded,
        vec![Message::Unknown {
            message_type: 0x50,
            flags: MessageFlags {
                hint: true,
                rfu: 0x5
            },
            data: original.slice(HEADER_SIZE..),
        }]
    );
    assert_eq!(encode(&decoded[0]), original);
}

#[test]
fn rfu_flag_bits_on_known_type_are_ignored() {
    // Section 7.1: a receiver MUST ignore the unassigned flag bits, so a
    // Bundle message with them set decodes as a plain Bundle.
    let mut pdu = BytesMut::new();
    pdu.put_u8(0x02);
    pdu.put_u8(0x50); // rfu bits 0b101, H clear
    pdu.put_u16(2);
    pdu.put_slice(b"hi");
    assert_eq!(
        decode_all(pdu.freeze()).unwrap(),
        vec![Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"hi")
        }]
    );
}

#[test]
fn malformed_hints_in_unknown_message_do_not_poison_pdu() {
    // Unknown messages are skipped via the Section 7 header length field
    // (Section 7.3).  Hint bytes that would not parse inside an unknown
    // message must not error the PDU; the following Bundle still decodes.
    let ok = Message::Bundle {
        hints: vec![],
        data: Bytes::from_static(b"ok"),
    };
    let mut pdu = BytesMut::new();
    pdu.put_u8(0x50);
    pdu.put_u8(0x80);
    pdu.put_u16(2);
    pdu.put_slice(b"\x1F\xFF");
    pdu.put_slice(&encode(&ok));

    assert_eq!(
        decode_all(pdu.freeze()).unwrap(),
        vec![
            Message::Unknown {
                message_type: 0x50,
                flags: MessageFlags { hint: true, rfu: 0 },
                data: Bytes::from_static(b"\x1F\xFF"),
            },
            ok,
        ]
    );
}

#[test]
fn malformed_bundle_length_hint_keeps_the_segment() {
    // A Bundle Length hint with a 3-byte value breaks Section 9.1, but the
    // segment is fully framed; it decodes with the hint carried as unknown
    // and its data intact.
    let mut pdu = BytesMut::new();
    pdu.put_u8(0x03); // Segment
    pdu.put_u8(0x80); // H
    pdu.put_u16(2 + 3 + 8 + 3);
    pdu.put_slice(&[BUNDLE_LENGTH_HINT << 1, 3, 1, 2, 3]);
    pdu.put_u32(7);
    pdu.put_u32(0);
    pdu.put_slice(b"abc");
    assert_eq!(
        decode_all(pdu.freeze()).unwrap(),
        vec![Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 7,
            segment_index: 0,
            hints: vec![HintItem::Unknown {
                hint_type: BUNDLE_LENGTH_HINT,
                value: Bytes::from_static(&[1, 2, 3]),
            }],
            data: Bytes::from_static(b"abc"),
        })]
    );
}

#[test]
fn unknown_cannot_carry_defined_or_reserved_type() {
    // Encoding an Unknown under a base-protocol or bundle-reserved type
    // would produce a message the decoder reads as something else.
    for t in [0x00u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x80, 0x9F] {
        let msg = Message::Unknown {
            message_type: t,
            flags: MessageFlags::default(),
            data: Bytes::from_static(b"x"),
        };
        let mut buf = BytesMut::new();
        assert_eq!(
            encode_message(&msg, &mut buf),
            Err(Error::NotAnUnknownType(t)),
            "type {t:#04x}"
        );
        assert!(buf.is_empty());
    }
    // The provisional FEC values are Private Use and relay fine.
    for t in 0x70u8..=0x73 {
        let msg = Message::Unknown {
            message_type: t,
            flags: MessageFlags::default(),
            data: Bytes::from_static(b"x"),
        };
        assert_eq!(decode_all(encode(&msg)).unwrap(), vec![msg]);
    }
}

#[test]
fn encode_errors_leave_the_buffer_untouched() {
    let mut buf = BytesMut::new();
    let too_long = Message::Bundle {
        hints: vec![],
        data: Bytes::from(vec![0u8; MAX_CONTENT_LENGTH + 1]),
    };
    assert_eq!(
        encode_message(&too_long, &mut buf),
        Err(Error::LengthOverflow {
            length: MAX_CONTENT_LENGTH + 1,
            max: MAX_CONTENT_LENGTH,
        })
    );
    assert!(buf.is_empty());

    let bad_hint = Message::TransferSegment(TransferSegmentMessage {
        transfer_number: 1,
        segment_index: 0,
        hints: vec![HintItem::Unknown {
            hint_type: 0x80,
            value: Bytes::new(),
        }],
        data: Bytes::from_static(b"x"),
    });
    assert_eq!(
        encode_message(&bad_hint, &mut buf),
        Err(Error::Hint(ValidationError::InvalidHintType(0x80)))
    );
    assert!(buf.is_empty());

    let bad_fec_hint = Message::PreAgreedFecSource(PreAgreedFecMessage {
        transfer_number: 1,
        fec_instance_id: 0,
        hints: vec![HintItem::Unknown {
            hint_type: 0x80,
            value: Bytes::new(),
        }],
        payload: Bytes::from_static(b"x"),
    });
    assert_eq!(
        encode_message(&bad_fec_hint, &mut buf),
        Err(Error::Hint(ValidationError::InvalidHintType(0x80)))
    );
    assert!(buf.is_empty());

    let too_long_unknown = Message::Unknown {
        message_type: 0x70,
        flags: MessageFlags::default(),
        data: Bytes::from(vec![0u8; MAX_CONTENT_LENGTH + 1]),
    };
    assert_eq!(
        encode_message(&too_long_unknown, &mut buf),
        Err(Error::LengthOverflow {
            length: MAX_CONTENT_LENGTH + 1,
            max: MAX_CONTENT_LENGTH,
        })
    );
    assert!(buf.is_empty());
}

#[test]
fn truncation_at_every_offset_keeps_the_whole_messages_before_it() {
    let messages = [
        cancel(1),
        Message::TransferSegment(TransferSegmentMessage {
            transfer_number: 2,
            segment_index: 0,
            hints: vec![HintItem::BundleLength(9)],
            data: Bytes::from_static(b"segment"),
        }),
        Message::Bundle {
            hints: vec![],
            data: Bytes::from_static(b"bundle"),
        },
    ];
    let mut pdu = BytesMut::new();
    let mut ends = Vec::new();
    for m in &messages {
        pdu.put_slice(&encode(m));
        ends.push(pdu.len());
    }
    let pdu = pdu.freeze();

    for cut in 0..=pdu.len() {
        // Every message wholly inside the cut decodes; a partial one is a
        // single InsufficientData naming the bytes its header (or, for a
        // partial header, the header itself) needs.
        let whole = ends.iter().take_while(|&&end| end <= cut).count();
        let mut expected: Vec<Result<Message>> =
            messages[..whole].iter().cloned().map(Ok).collect();
        let start = whole.checked_sub(1).map_or(0, |i| ends[i]);
        if cut > start {
            let needed = if cut - start < HEADER_SIZE {
                start + HEADER_SIZE
            } else {
                ends[whole]
            };
            expected.push(Err(Error::InsufficientData {
                needed,
                available: cut,
            }));
        }
        assert_eq!(
            decode_pdu(pdu.slice(..cut)).collect::<Vec<_>>(),
            expected,
            "cut at {cut}"
        );
    }
}

#[test]
fn encoded_message_len_accurate() {
    let mut messages = vec![
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
        Message::TransferEnd(TransferSegmentMessage {
            transfer_number: 1,
            segment_index: 1,
            hints: vec![HintItem::Unknown {
                hint_type: 3,
                value: Bytes::from_static(b"zz"),
            }],
            data: Bytes::from_static(b"end"),
        }),
        cancel(1),
        Message::Unknown {
            message_type: 0x50,
            flags: MessageFlags::default(),
            data: Bytes::from_static(b"opaque"),
        },
    ];
    messages.extend(fec_messages());
    for msg in &messages {
        assert_eq!(encode(msg).len(), encoded_message_len(msg), "{msg:?}");
    }
}
