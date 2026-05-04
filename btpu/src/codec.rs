use crate::fec;
use crate::header::{self, HEADER_SIZE, MAX_CONTENT_LENGTH, MessageHeader};
use crate::hint::{self, HintItem};
use crate::message::*;
use alloc::vec::Vec;
use bytes::{BufMut, Bytes, BytesMut};

/// Errors from message encoding and decoding.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The message type byte is not recognized.
    #[error("Invalid message type {0:#04x}")]
    InvalidMessageType(u8),

    /// The message type byte is in a reserved range (BPv6 or BPv7 CBOR).
    #[error("Reserved message type {0:#04x}")]
    ReservedMessageType(u8),

    /// A message content length exceeds the 20-bit maximum.
    #[error("Message content length {length} exceeds 20-bit maximum ({max})")]
    LengthOverflow { length: usize, max: usize },

    /// Not enough data to decode a message or header.
    #[error("Insufficient data: need {needed} bytes, have {available}")]
    InsufficientData { needed: usize, available: usize },

    /// A Bundle Length hint has an invalid size (must be 1, 2, 4, or 8).
    #[error("Invalid Bundle Length hint size {0} (must be 1, 2, 4, or 8)")]
    InvalidBundleLengthHintSize(u8),

    /// A hint type exceeds the 7-bit maximum.
    #[error("Invalid hint type {0:#04x} (must be <= 0x7f)")]
    InvalidHintType(u8),

    /// A hint value exceeds the 255-byte maximum of the 8-bit length field.
    #[error("Hint value length {length} exceeds maximum {max}")]
    HintValueOverflow { length: usize, max: usize },
}

/// Decode all messages from a single convergence layer PDU.
///
/// Indefinite Padding (zero bytes) is consumed silently.  Unknown message
/// types are preserved as [`Message::Unknown`].
///
/// **Frame classification:** if the first byte of `pdu` is in the
/// bundle-reserved range (`0x06` for BPv6, `0x80..=0x9F` for BPv7), the
/// entire `pdu` is treated as a bare bundle frame on a shared link and
/// returned as a single [`Message::Bundle`] containing the frame bytes
/// verbatim. This lets a CLA process every received frame through one path —
/// see [`frame_kind`] for the classifier itself, useful for callers that
/// want to peek without decoding (e.g. per-protocol metrics).
///
/// Reserved bytes encountered *mid-PDU* (after at least one BTP-U message
/// has been parsed) still error with [`Error::ReservedMessageType`], since a
/// well-formed BTP-U PDU never contains those bytes mid-stream.
pub fn decode_pdu(pdu: Bytes) -> Result<Vec<Message>, Error> {
    // Bare bundle frame on a shared link: short-circuit and hand it back as
    // a single Bundle message. The input `pdu` becomes the message's `data`
    // directly — no heap copy.
    match frame_kind(&pdu) {
        FrameKind::Bpv6Bundle | FrameKind::Bpv7Bundle => {
            return Ok(alloc::vec![Message::Bundle {
                hints: Vec::new(),
                data: pdu,
            }]);
        }
        FrameKind::BtpuPdu => {}
    }

    let mut messages = Vec::new();
    let mut offset = 0;

    while offset < pdu.len() {
        // Indefinite padding: skip consecutive zero bytes (Section 8.6).
        if pdu[offset] == 0 {
            while offset < pdu.len() && pdu[offset] == 0 {
                offset += 1;
            }
            // Don't emit an IndefinitePadding variant for each run; it has no
            // semantic meaning and the receiver MUST ignore it.
            continue;
        }

        // Decode the 4-byte header.
        let hdr = header::decode_header(&pdu[offset..])?;
        let content_start = offset + HEADER_SIZE;
        let content_end = content_start + hdr.length as usize;

        if content_end > pdu.len() {
            return Err(Error::InsufficientData {
                needed: content_end,
                available: pdu.len(),
            });
        }

        let content = &pdu[content_start..content_end];
        let msg = decode_message(hdr.message_type, hdr.flags, content, &pdu)?;
        messages.push(msg);
        offset = content_end;
    }

    Ok(messages)
}

fn decode_message(
    message_type: u8,
    flags: MessageFlags,
    content: &[u8],
    pdu: &Bytes,
) -> Result<Message, Error> {
    // Resolve the message type BEFORE parsing anything from the content.
    // Section 8.5: unknown messages are skipped via the header length field
    // and preserved opaquely -- their content (hint bytes included) is not
    // interpreted, so a malformed or extension-defined hint chain in an
    // unknown message cannot error the rest of the PDU.
    let mt = match MessageType::try_from(message_type) {
        Ok(mt) => mt,
        Err(_) if is_reserved_bpv6(message_type) || is_reserved_bpv7(message_type) => {
            return Err(Error::ReservedMessageType(message_type));
        }
        Err(_) => {
            return Ok(Message::Unknown {
                message_type,
                flags,
                data: pdu.slice_ref(content),
            });
        }
    };

    // Padding content is ignored wholesale (Section 8.6); never parse it.
    if mt == MessageType::DefinitePadding {
        return Ok(Message::DefinitePadding(content.len()));
    }

    // Parse hints if H flag is set.
    let (hints, data_offset) = if flags.hint {
        let (items, consumed) = hint::decode_hints(content, pdu)?;
        (items, consumed)
    } else {
        (Vec::new(), 0)
    };
    let data = &content[data_offset..];

    match mt {
        MessageType::DefinitePadding => unreachable!("definite padding handled above"),

        MessageType::Bundle => Ok(Message::Bundle {
            hints,
            data: pdu.slice_ref(data),
        }),

        MessageType::TransferSegment => {
            let (transfer_number, segment_index, segment_data) = decode_transfer_fields(data, pdu)?;
            Ok(Message::TransferSegment(TransferSegmentMessage {
                transfer_number,
                segment_index,
                hints,
                data: segment_data,
            }))
        }

        MessageType::TransferEnd => {
            let (transfer_number, segment_index, segment_data) = decode_transfer_fields(data, pdu)?;
            Ok(Message::TransferEnd(TransferEndMessage {
                transfer_number,
                segment_index,
                hints,
                data: segment_data,
            }))
        }

        MessageType::TransferCancel => {
            if data.len() < 4 {
                return Err(Error::InsufficientData {
                    needed: 4,
                    available: data.len(),
                });
            }
            let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            Ok(Message::TransferCancel { transfer_number })
        }

        MessageType::PreAgreedFecSource => {
            if data.len() < 5 {
                return Err(Error::InsufficientData {
                    needed: 5,
                    available: data.len(),
                });
            }
            let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let fec_instance_id = data[4];
            // The scheme-defined internal boundaries of the payload are not
            // knowable here; it is kept opaque (see the struct docs).
            let payload = pdu.slice_ref(&data[5..]);
            Ok(Message::PreAgreedFecSource(
                fec::PreAgreedFecSourceMessage {
                    transfer_number,
                    fec_instance_id,
                    hints,
                    payload,
                },
            ))
        }

        MessageType::ExplicitFecSource => {
            if data.len() < 5 {
                return Err(Error::InsufficientData {
                    needed: 5,
                    available: data.len(),
                });
            }
            let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let fec_encoding_id = data[4];
            let payload = pdu.slice_ref(&data[5..]);
            Ok(Message::ExplicitFecSource(fec::ExplicitFecSourceMessage {
                transfer_number,
                fec_encoding_id,
                hints,
                payload,
            }))
        }

        MessageType::PreAgreedFecRepair => {
            if data.len() < 5 {
                return Err(Error::InsufficientData {
                    needed: 5,
                    available: data.len(),
                });
            }
            let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let fec_instance_id = data[4];
            let payload = pdu.slice_ref(&data[5..]);
            Ok(Message::PreAgreedFecRepair(
                fec::PreAgreedFecRepairMessage {
                    transfer_number,
                    fec_instance_id,
                    hints,
                    payload,
                },
            ))
        }

        MessageType::ExplicitFecRepair => {
            if data.len() < 5 {
                return Err(Error::InsufficientData {
                    needed: 5,
                    available: data.len(),
                });
            }
            let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let fec_encoding_id = data[4];
            let payload = pdu.slice_ref(&data[5..]);
            Ok(Message::ExplicitFecRepair(fec::ExplicitFecRepairMessage {
                transfer_number,
                fec_encoding_id,
                hints,
                payload,
            }))
        }

        MessageType::IndefinitePadding => unreachable!(
            "indefinite padding is consumed by decode_pdu before reaching decode_message"
        ),
    }
}

/// Parse the common transfer_number (u32) + segment_index (u32) prefix.
fn decode_transfer_fields(data: &[u8], pdu: &Bytes) -> Result<(u32, u32, Bytes), Error> {
    if data.len() < 8 {
        return Err(Error::InsufficientData {
            needed: 8,
            available: data.len(),
        });
    }
    let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let segment_index = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let segment_data = pdu.slice_ref(&data[8..]);
    Ok((transfer_number, segment_index, segment_data))
}

/// Returns the total encoded size of a message (header + hints + content).
pub fn encoded_message_len(message: &Message) -> usize {
    HEADER_SIZE + message_content_len(message)
}

/// Returns the content length (everything after the 4-byte header).
fn message_content_len(message: &Message) -> usize {
    match message {
        Message::IndefinitePadding => 0, // special: no header
        Message::DefinitePadding(n) => *n,
        Message::Bundle { hints, data } => hint::encoded_hints_len(hints) + data.len(),
        Message::TransferSegment(m) => hint::encoded_hints_len(&m.hints) + 8 + m.data.len(),
        Message::TransferEnd(m) => hint::encoded_hints_len(&m.hints) + 8 + m.data.len(),
        Message::TransferCancel { .. } => 4,
        Message::PreAgreedFecSource(m) => {
            hint::encoded_hints_len(&m.hints) + 4 + 1 + m.payload.len()
        }
        Message::ExplicitFecSource(m) => {
            hint::encoded_hints_len(&m.hints) + 4 + 1 + m.payload.len()
        }
        Message::PreAgreedFecRepair(m) => {
            hint::encoded_hints_len(&m.hints) + 4 + 1 + m.payload.len()
        }
        Message::ExplicitFecRepair(m) => {
            hint::encoded_hints_len(&m.hints) + 4 + 1 + m.payload.len()
        }
        Message::Unknown { data, .. } => data.len(),
    }
}

/// Encode a single message into `dst`.
pub fn encode_message(message: &Message, dst: &mut BytesMut) -> Result<(), Error> {
    match message {
        Message::IndefinitePadding => {
            dst.put_u8(MessageType::IndefinitePadding.into());
        }

        Message::DefinitePadding(n) => {
            let content_len = *n;
            check_content_length(content_len)?;
            let flags = MessageFlags::default();
            write_header(
                MessageType::DefinitePadding.into(),
                flags,
                content_len as u32,
                dst,
            );
            dst.put_bytes(0, content_len);
        }

        Message::Bundle { hints, data } => {
            let hints_len = hint::encoded_hints_len(hints);
            let content_len = hints_len + data.len();
            check_content_length(content_len)?;
            hint::validate_hints(hints)?;
            let flags = MessageFlags {
                hint: !hints.is_empty(),
            };
            write_header(MessageType::Bundle.into(), flags, content_len as u32, dst);
            hint::encode_hints(hints, dst)?;
            dst.put_slice(data);
        }

        Message::TransferSegment(m) => {
            encode_transfer_message(
                MessageType::TransferSegment.into(),
                m.transfer_number,
                m.segment_index,
                &m.hints,
                &m.data,
                dst,
            )?;
        }

        Message::TransferEnd(m) => {
            encode_transfer_message(
                MessageType::TransferEnd.into(),
                m.transfer_number,
                m.segment_index,
                &m.hints,
                &m.data,
                dst,
            )?;
        }

        Message::TransferCancel { transfer_number } => {
            check_content_length(4)?;
            write_header(
                MessageType::TransferCancel.into(),
                MessageFlags::default(),
                4,
                dst,
            );
            dst.put_u32(*transfer_number);
        }

        Message::PreAgreedFecSource(m) => {
            encode_fec_message(
                MessageType::PreAgreedFecSource,
                &m.hints,
                m.transfer_number,
                m.fec_instance_id,
                &m.payload,
                dst,
            )?;
        }

        Message::ExplicitFecSource(m) => {
            encode_fec_message(
                MessageType::ExplicitFecSource,
                &m.hints,
                m.transfer_number,
                m.fec_encoding_id,
                &m.payload,
                dst,
            )?;
        }

        Message::PreAgreedFecRepair(m) => {
            encode_fec_message(
                MessageType::PreAgreedFecRepair,
                &m.hints,
                m.transfer_number,
                m.fec_instance_id,
                &m.payload,
                dst,
            )?;
        }

        Message::ExplicitFecRepair(m) => {
            encode_fec_message(
                MessageType::ExplicitFecRepair,
                &m.hints,
                m.transfer_number,
                m.fec_encoding_id,
                &m.payload,
                dst,
            )?;
        }

        Message::Unknown {
            message_type,
            flags,
            data,
        } => {
            check_content_length(data.len())?;
            write_header(*message_type, *flags, data.len() as u32, dst);
            dst.put_slice(data);
        }
    }
    Ok(())
}

fn encode_transfer_message(
    msg_type: u8,
    transfer_number: u32,
    segment_index: u32,
    hints: &[HintItem],
    data: &Bytes,
    dst: &mut BytesMut,
) -> Result<(), Error> {
    let hints_len = hint::encoded_hints_len(hints);
    let content_len = hints_len + 8 + data.len();
    check_content_length(content_len)?;
    hint::validate_hints(hints)?;
    let flags = MessageFlags {
        hint: !hints.is_empty(),
    };
    write_header(msg_type, flags, content_len as u32, dst);
    hint::encode_hints(hints, dst)?;
    dst.put_u32(transfer_number);
    dst.put_u32(segment_index);
    dst.put_slice(data);
    Ok(())
}

fn write_header(message_type: u8, flags: MessageFlags, length: u32, dst: &mut BytesMut) {
    let start = dst.len();
    dst.put_bytes(0, HEADER_SIZE);
    header::encode_header(
        &MessageHeader {
            message_type,
            flags,
            length,
        },
        &mut dst[start..],
    );
}

/// Encode one of the four FEC messages; they share a single wire shape:
/// hints, transfer number, instance/encoding ID byte, then the scheme-opaque
/// payload.
fn encode_fec_message(
    message_type: MessageType,
    hints: &[hint::HintItem],
    transfer_number: u32,
    id: u8,
    payload: &Bytes,
    dst: &mut BytesMut,
) -> Result<(), Error> {
    let hints_len = hint::encoded_hints_len(hints);
    let content_len = hints_len + 4 + 1 + payload.len();
    check_content_length(content_len)?;
    hint::validate_hints(hints)?;
    let flags = MessageFlags {
        hint: !hints.is_empty(),
    };
    write_header(message_type.into(), flags, content_len as u32, dst);
    hint::encode_hints(hints, dst)?;
    dst.put_u32(transfer_number);
    dst.put_u8(id);
    dst.put_slice(payload);
    Ok(())
}

fn check_content_length(len: usize) -> Result<(), Error> {
    if len > MAX_CONTENT_LENGTH {
        return Err(Error::LengthOverflow {
            length: len,
            max: MAX_CONTENT_LENGTH,
        });
    }
    Ok(())
}

/// Pad `dst` to `target_len` bytes.
///
/// Uses Definite Padding for >= 4 bytes of remaining space, then Indefinite
/// Padding (zeros) for any remaining 1-3 bytes, per spec recommendation.
pub fn pad_pdu(dst: &mut BytesMut, target_len: usize) {
    let current = dst.len();
    if current >= target_len {
        return;
    }

    let remaining = target_len - current;
    if remaining >= HEADER_SIZE {
        // Definite Padding: header (4 bytes) + zero-filled content
        let content_len = remaining - HEADER_SIZE;
        write_header(
            MessageType::DefinitePadding.into(),
            MessageFlags::default(),
            content_len as u32,
            dst,
        );
        dst.put_bytes(0, content_len);
    } else {
        // Indefinite Padding: just zero bytes
        dst.put_bytes(0, remaining);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

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
            let decoded = decode_pdu(buf.freeze()).unwrap();
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
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
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
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
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
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
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
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
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
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
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
        let msg = Message::DefinitePadding(10);
        let mut buf = BytesMut::new();
        encode_message(&msg, &mut buf).unwrap();
        assert_eq!(buf.len(), HEADER_SIZE + 10);
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
        assert_eq!(decoded.len(), 1);
        matches!(&decoded[0], Message::DefinitePadding(10));
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
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
        assert_eq!(decoded.len(), 1);
        matches!(&decoded[0], Message::Bundle { .. });
    }

    #[test]
    fn all_zeros_pdu() {
        let pdu = Bytes::from(vec![0u8; 64]);
        let decoded = decode_pdu(pdu).unwrap();
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
            Message::DefinitePadding(2),
        ];
        let mut buf = BytesMut::new();
        for m in &msgs {
            encode_message(m, &mut buf).unwrap();
        }
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
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
        let decoded = decode_pdu(buf.clone().freeze()).unwrap();
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
    fn bare_bpv6_bundle_decoded_as_bundle_message() {
        // A frame starting with the BPv6 reserved byte is treated as a bare
        // bundle and returned as a single Message::Bundle containing the
        // whole frame verbatim.
        let frame = Bytes::from_static(&[0x06, 0xDE, 0xAD, 0xBE, 0xEF]);
        let decoded = decode_pdu(frame.clone()).unwrap();
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
            let decoded = decode_pdu(frame.clone()).unwrap();
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
    fn mid_pdu_reserved_byte_still_errors() {
        // A well-formed PDU never contains reserved bytes mid-stream. If
        // parsing has already consumed at least one message and the next
        // message's type byte is reserved, that's malformed BTP-U — error.
        let mut buf = BytesMut::new();
        // First message: a TransferCancel (8 bytes total: 4-byte header + 4-byte transfer_number)
        encode_message(&Message::TransferCancel { transfer_number: 1 }, &mut buf).unwrap();
        // Then append a header whose message_type byte is 0x06 (reserved).
        let mut hdr = [0u8; HEADER_SIZE];
        header::encode_header(
            &MessageHeader {
                message_type: 0x06,
                flags: MessageFlags::default(),
                length: 0,
            },
            &mut hdr,
        );
        buf.extend_from_slice(&hdr);
        let result = decode_pdu(buf.freeze());
        assert!(matches!(result, Err(Error::ReservedMessageType(0x06))));
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
        let decoded = decode_pdu(pdu.freeze()).unwrap();
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

        let decoded = decode_pdu(original.clone()).unwrap();
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
    fn malformed_hints_in_unknown_message_do_not_poison_pdu() {
        // Section 8.5: unknown messages are skipped via the length field.
        // A hint chain we cannot parse (here: invalid Bundle Length size)
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

        let decoded = decode_pdu(pdu.freeze()).unwrap();
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
            Message::DefinitePadding(10),
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
}
