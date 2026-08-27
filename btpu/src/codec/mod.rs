use alloc::vec::Vec;
use core::iter::FusedIterator;

use bytes::{BufMut, Bytes, BytesMut};

use self::{
    header::{HEADER_SIZE, MAX_CONTENT_LENGTH, MessageHeader},
    hint::HintItem,
    message::{
        FrameKind, Message, MessageFlags, MessageType, TransferEndMessage, TransferSegmentMessage,
        frame_kind, is_reserved_bpv6, is_reserved_bpv7,
    },
};
use crate::fec;

mod error;
pub mod header;
pub mod hint;
pub mod message;

pub use self::error::{Error, Result};

/// Lazily decode the messages in a single convergence layer PDU.
///
/// Returns a [`MessageIter`] yielding one [`Result<Message>`](Result) per
/// message; nothing is parsed until the iterator is advanced. Faults are
/// contained to the smallest unit the wire format allows: the Section 7
/// header length bounds every message, so a message whose extent is known
/// but whose interior is malformed yields an [`Err`] and iteration continues
/// at the next message boundary, while a framing fault (truncated header,
/// length past the buffer, or a bundle-reserved type byte mid-stream) yields
/// a final [`Err`] and stops iteration permanently — without a boundary the
/// remaining bytes cannot be walked. [`MessageIter::is_exhausted`]
/// distinguishes the two after an error.
///
/// Indefinite Padding (zero bytes) is consumed silently.  Unknown message
/// types are preserved as [`Message::Unknown`].
///
/// **Frame classification:** if the first byte of `pdu` is in the
/// bundle-reserved range (`0x06` for BPv6, `0x80..=0x9F` for BPv7), the
/// entire `pdu` is treated as a bare bundle frame on a shared link and
/// yielded as a single [`Message::Bundle`] containing the frame bytes
/// verbatim. This lets a CLA process every received frame through one path —
/// see [`frame_kind`] for the classifier itself, useful for callers that
/// want to peek without decoding (e.g. per-protocol metrics).
///
/// A reserved byte encountered *mid-PDU* (after at least one BTP-U message
/// has been parsed) marks raw bundle bytes (the ranges are reserved for
/// exactly that distinction, Section 12.1), but a bundle carries no BTP-U
/// header to give its extent and this decoder does not parse bundle formats
/// to find one. It is therefore a terminal framing error
/// ([`Error::ReservedMessageType`]): messages already yielded are
/// unaffected, and the unwalkable remainder is discarded.
pub fn decode_pdu(pdu: Bytes) -> MessageIter {
    let bare_bundle = matches!(
        frame_kind(&pdu),
        FrameKind::Bpv6Bundle | FrameKind::Bpv7Bundle
    );
    MessageIter {
        pdu,
        offset: 0,
        bare_bundle,
        done: false,
    }
}

/// Lazy message iterator over a PDU, returned by [`decode_pdu`].
///
/// Owns the PDU [`Bytes`], so yielded messages hold zero-copy views into it.
/// An [`Err`] for a message whose extent was known is recoverable: iteration
/// resumes at the next message boundary. An [`Err`] from the framing itself
/// exhausts the iterator: the stream position is unreliable, so no further
/// messages are parsed.
#[derive(Debug)]
pub struct MessageIter {
    pdu: Bytes,
    offset: usize,
    /// The PDU is a bare bundle frame on a shared link; yield it once as a
    /// single Bundle message. The `pdu` becomes the message's `data`
    /// directly — no heap copy.
    bare_bundle: bool,
    done: bool,
}

impl MessageIter {
    /// Whether the iterator has stopped permanently: either the PDU was
    /// fully consumed, or a framing fault made the remainder undecodable
    /// and it was discarded.
    ///
    /// Checked immediately after an [`Err`] item, this tells whether the
    /// fault was contained to one skipped message (`false`) or cost the rest
    /// of the PDU (`true`).
    pub fn is_exhausted(&self) -> bool {
        self.done
    }

    /// Framing stage: locate the extent of the message at `offset`.
    ///
    /// Errors here are terminal for the whole PDU — a bundle-reserved type
    /// byte, a truncated header, or a length past the buffer all mean the
    /// next message boundary cannot be determined.
    fn frame_next(&self) -> Result<(MessageHeader, usize)> {
        // A bundle-reserved byte mid-stream marks raw bundle bytes (Section
        // 12.1 reserves the ranges for that distinction), which carry no
        // BTP-U header to give their extent, and this decoder does not parse
        // bundle formats to find one.
        let message_type = self.pdu[self.offset];
        if is_reserved_bpv6(message_type) || is_reserved_bpv7(message_type) {
            return Err(Error::ReservedMessageType(message_type));
        }

        let hdr = header::decode_header(&self.pdu[self.offset..])?;
        let content_end = self.offset + HEADER_SIZE + hdr.length as usize;
        if content_end > self.pdu.len() {
            return Err(Error::InsufficientData {
                needed: content_end,
                available: self.pdu.len(),
            });
        }
        Ok((hdr, content_end))
    }
}

impl Iterator for MessageIter {
    type Item = Result<Message>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        if self.bare_bundle {
            self.done = true;
            return Some(Ok(Message::Bundle {
                hints: Vec::new(),
                data: self.pdu.clone(),
            }));
        }

        // Indefinite padding: skip consecutive zero bytes (Section 8.6).
        // No IndefinitePadding variant is emitted for a run; it has no
        // semantic meaning and the receiver MUST ignore it.
        while self.offset < self.pdu.len() && self.pdu[self.offset] == 0 {
            self.offset += 1;
        }
        if self.offset == self.pdu.len() {
            self.done = true;
            return None;
        }

        // Framing failures are terminal: without a message boundary the
        // remainder cannot be walked.
        let (hdr, content_end) = match self.frame_next() {
            Ok(frame) => frame,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };

        // The extent is known, so an interior fault is contained to this one
        // message: skip to the next boundary the header length gives and
        // keep iterating.
        let content = &self.pdu[self.offset + HEADER_SIZE..content_end];
        let result = decode_message(hdr.message_type, hdr.flags, content, &self.pdu);
        self.offset = content_end;
        Some(result)
    }
}

impl FusedIterator for MessageIter {}

fn decode_message(
    message_type: u8,
    flags: MessageFlags,
    content: &[u8],
    pdu: &Bytes,
) -> Result<Message> {
    // Resolve the message type BEFORE parsing anything from the content.
    // Every message is self-framing (Section 7 TLV header), so an unknown
    // type is skipped via the header length field and preserved opaquely:
    // its content (hint bytes included) is not interpreted, so a malformed
    // or extension-defined hint chain in an unknown message cannot error the
    // rest of the PDU.  Bundle-reserved type bytes never reach here; the
    // framing stage rejects them.
    let Ok(mt) = MessageType::try_from(message_type) else {
        return Ok(Message::Unknown {
            message_type,
            flags,
            data: pdu.slice_ref(content),
        });
    };

    // Receivers MUST ignore Definite Padding content (Section 8.5); never
    // parse it.
    if mt == MessageType::DefinitePadding {
        return Ok(Message::DefinitePadding { len: content.len() });
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
            let (transfer_number, fec_instance_id, payload) = decode_fec_fields(data, pdu)?;
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
            let (transfer_number, fec_encoding_id, payload) = decode_fec_fields(data, pdu)?;
            Ok(Message::ExplicitFecSource(fec::ExplicitFecSourceMessage {
                transfer_number,
                fec_encoding_id,
                hints,
                payload,
            }))
        }

        MessageType::PreAgreedFecRepair => {
            let (transfer_number, fec_instance_id, payload) = decode_fec_fields(data, pdu)?;
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
            let (transfer_number, fec_encoding_id, payload) = decode_fec_fields(data, pdu)?;
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
fn decode_transfer_fields(data: &[u8], pdu: &Bytes) -> Result<(u32, u32, Bytes)> {
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

/// Parse the common transfer_number (u32) + instance/encoding ID (u8) prefix
/// shared by all four FEC messages.  The remaining payload is kept opaque:
/// its scheme-defined internal boundaries are not knowable here (see the
/// struct docs).
fn decode_fec_fields(data: &[u8], pdu: &Bytes) -> Result<(u32, u8, Bytes)> {
    if data.len() < 5 {
        return Err(Error::InsufficientData {
            needed: 5,
            available: data.len(),
        });
    }
    let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let id = data[4];
    let payload = pdu.slice_ref(&data[5..]);
    Ok((transfer_number, id, payload))
}

/// Returns the total encoded size of a message, exactly matching what
/// [`encode_message`] writes: header + hints + content — except Indefinite
/// Padding, which is a single zero byte with no header (Section 8.6).
pub fn encoded_message_len(message: &Message) -> usize {
    match message {
        Message::IndefinitePadding => 1,
        _ => HEADER_SIZE + message_content_len(message),
    }
}

/// Returns the content length (everything after the 4-byte header).
fn message_content_len(message: &Message) -> usize {
    match message {
        // Headerless; sized directly in encoded_message_len.
        Message::IndefinitePadding => 0,
        Message::DefinitePadding { len } => *len,
        Message::Bundle { hints, data } => hint::encoded_hints_len(hints) + data.len(),
        Message::TransferSegment(m) => hint::encoded_hints_len(&m.hints) + 8 + m.data.len(),
        Message::TransferEnd(m) => hint::encoded_hints_len(&m.hints) + 8 + m.data.len(),
        Message::TransferCancel { .. } => 4,
        // The four FEC messages share one wire shape: hints, transfer
        // number, ID byte, then the scheme-opaque payload.
        Message::PreAgreedFecSource(fec::PreAgreedFecSourceMessage { hints, payload, .. })
        | Message::ExplicitFecSource(fec::ExplicitFecSourceMessage { hints, payload, .. })
        | Message::PreAgreedFecRepair(fec::PreAgreedFecRepairMessage { hints, payload, .. })
        | Message::ExplicitFecRepair(fec::ExplicitFecRepairMessage { hints, payload, .. }) => {
            hint::encoded_hints_len(hints) + 4 + 1 + payload.len()
        }
        Message::Unknown { data, .. } => data.len(),
    }
}

/// Encode a single message into `dst`.
pub fn encode_message(message: &Message, dst: &mut BytesMut) -> Result<()> {
    match message {
        Message::IndefinitePadding => {
            dst.put_u8(MessageType::IndefinitePadding.into());
        }

        Message::DefinitePadding { len } => {
            let content_len = *len;
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
                rfu: 0,
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
) -> Result<()> {
    let hints_len = hint::encoded_hints_len(hints);
    let content_len = hints_len + 8 + data.len();
    check_content_length(content_len)?;
    hint::validate_hints(hints)?;
    let flags = MessageFlags {
        hint: !hints.is_empty(),
        rfu: 0,
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
) -> Result<()> {
    let hints_len = hint::encoded_hints_len(hints);
    let content_len = hints_len + 4 + 1 + payload.len();
    check_content_length(content_len)?;
    hint::validate_hints(hints)?;
    let flags = MessageFlags {
        hint: !hints.is_empty(),
        rfu: 0,
    };
    write_header(message_type.into(), flags, content_len as u32, dst);
    hint::encode_hints(hints, dst)?;
    dst.put_u32(transfer_number);
    dst.put_u8(id);
    dst.put_slice(payload);
    Ok(())
}

fn check_content_length(len: usize) -> Result<()> {
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
/// Space beyond a single message's 20-bit length field is filled with a
/// chain of maximum-size Definite Padding messages (padding is valid at any
/// point in a PDU, Section 3.2), so every target length is reachable and the
/// emitted headers are always truthful.
pub fn pad_pdu(dst: &mut BytesMut, target_len: usize) {
    while dst.len() < target_len {
        let remaining = target_len - dst.len();
        if remaining >= HEADER_SIZE {
            // Definite Padding: header (4 bytes) + zero-filled content,
            // capped to what the 20-bit length field can declare.
            let content_len = (remaining - HEADER_SIZE).min(MAX_CONTENT_LENGTH);
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
}
