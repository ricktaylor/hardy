use alloc::vec::Vec;
use core::{fmt, iter::FusedIterator};

use bytes::{BufMut, Bytes, BytesMut};

use self::{
    header::{HEADER_SIZE, MAX_CONTENT_LENGTH, MessageHeader, decode_header, encode_header},
    hint::{HintItem, decode_hints, encoded_hints_len, validate_hints, write_hints},
    message::{FrameKind, Message, MessageFlags, MessageType, TransferSegmentMessage, frame_kind},
};
use crate::fec::{ExplicitFecMessage, PreAgreedFecMessage};

mod error;
pub mod header;
pub mod hint;
pub mod message;

pub use self::error::{Error, Result};

/// Finds the extent of an encapsulated bundle so the decoder can step over
/// it: the caller's "peek" into a bundle format this crate does not parse.
///
/// BTP-U reserves the message-type values that begin a bundle (0x06 for
/// BPv6, 0x80..=0x9F for BPv7, Section 12.1) so that a bundle in its native
/// format can appear where a message is expected (Section 7.3).  Both bundle
/// formats are self-delimiting, but only to a receiver that implements them.
/// A CLA that holds a bundle parser supplies one of these through
/// [`DecodeOptions::bundle_extent`]; the decoder then delivers exactly the
/// bundle's bytes and continues with whatever follows, which is what
/// Section 7.3 asks of a receiver that implements the format.
///
/// Any `Fn(&[u8]) -> Option<usize>` implements the trait, and a reference
/// to one coerces to the `&dyn BundleExtent` the options hold:
///
/// ```
/// # use bytes::Bytes;
/// # use hardy_btpu::codec::{DecodeOptions, decode_pdu_with, message::Message};
/// // A stand-in for a bundle parser: every BPv7 bundle here is 3 bytes.
/// let extent = |bytes: &[u8]| (bytes.len() >= 3).then_some(3);
/// let options = DecodeOptions {
///     bundle_extent: Some(&extent),
///     ..DecodeOptions::default()
/// };
/// // The bundle, then link padding the hook lets the decoder skip.
/// let pdu = Bytes::from_static(&[0x9F, 0xAA, 0xFF, 0, 0, 0]);
/// let mut messages = decode_pdu_with(pdu, options);
/// assert_eq!(
///     messages.next(),
///     Some(Ok(Message::Bundle {
///         hints: vec![],
///         data: Bytes::from_static(&[0x9F, 0xAA, 0xFF]),
///     }))
/// );
/// assert_eq!(messages.next(), None);
/// ```
///
/// Once a hook is supplied, the decoder relies on it alone: a bundle it
/// declines is never taken to fill the rest of the PDU, even where the
/// decoder would do so without a hook, since a hook that cannot delimit
/// the bundle is better evidence than the bare-frame guess.
pub trait BundleExtent {
    /// The length in bytes of the bundle that begins at `bytes[0]`, or
    /// `None` if `bytes` does not begin a bundle this implementation can
    /// delimit (including a bundle truncated by the end of `bytes`).
    ///
    /// `bytes` runs from the bundle's first byte to the end of the PDU, so
    /// it may hold further messages or link padding after the bundle.
    ///
    /// The decoder ends the PDU with [`Error::EncapsulatedBundle`] on
    /// `None` or `Some(0)` (a zero-length bundle cannot advance it), and
    /// with [`Error::InsufficientData`] on a length past the end of
    /// `bytes`.  If this panics, the [`MessageIter`] is left where it was,
    /// so advancing it again calls the hook on the same bytes.
    fn bundle_extent(&self, bytes: &[u8]) -> Option<usize>;
}

impl<F: Fn(&[u8]) -> Option<usize>> BundleExtent for F {
    fn bundle_extent(&self, bytes: &[u8]) -> Option<usize> {
        self(bytes)
    }
}

/// Options for [`decode_pdu_with`].  The default decodes the base protocol
/// only and has no bundle-extent hook.
#[derive(Clone, Copy, Default)]
pub struct DecodeOptions<'a> {
    /// Interpret the four provisional FEC message types (0x70..=0x73, see
    /// [`MessageType`]).  Off by default: the values are in the Private Use
    /// range, so a link whose peer assigns them privately must be left to
    /// relay them as [`Message::Unknown`].
    pub fec: bool,
    /// How to find the extent of an encapsulated bundle (Section 7.3).
    /// Without one, a bundle-reserved first byte makes the whole PDU the
    /// bundle, and one found after a message is a terminal
    /// [`Error::EncapsulatedBundle`]; see [`decode_pdu`] for the padding
    /// consequences.
    pub bundle_extent: Option<&'a dyn BundleExtent>,
}

impl fmt::Debug for DecodeOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodeOptions")
            .field("fec", &self.fec)
            .field("bundle_extent", &self.bundle_extent.is_some())
            .finish()
    }
}

/// Lazily decode the messages in a single convergence layer PDU with the
/// default [`DecodeOptions`]: base protocol only, no bundle-extent hook.
///
/// Returns a [`MessageIter`] yielding one [`Result<Message>`](Result) per
/// message; nothing is parsed until the iterator is advanced. Faults are
/// contained to the smallest unit the wire format allows: the Section 7
/// header length bounds every message, so a message whose extent is known
/// but whose interior is malformed yields an [`Err`] and iteration continues
/// at the next message boundary (the skip-and-continue rule of Section 7.3),
/// while a framing fault (truncated header, length past the buffer, or an
/// encapsulated bundle of unknown extent) yields a final [`Err`] and stops
/// iteration permanently, since without a boundary the remaining bytes
/// cannot be walked. [`MessageIter::is_exhausted`] distinguishes the two
/// after an error.
///
/// Indefinite Padding (zero bytes) is consumed silently.  Unknown message
/// types are preserved as [`Message::Unknown`].
///
/// # Encapsulated bundles and padding
///
/// A bundle in its native format may appear wherever a message may
/// (Section 7.3); it is recognised by its first byte (0x06 for BPv6,
/// 0x80..=0x9F for BPv7, Section 12.1), and a receiver that can delimit it
/// delivers it as a [`Message::Bundle`] and continues.  This crate does not
/// parse bundle formats, so how far it gets depends on whether the caller
/// supplied a [`DecodeOptions::bundle_extent`] via [`decode_pdu_with`]:
///
/// - **With a hook**, the bundle is exactly the bytes the hook measures,
///   whatever precedes or follows it: leading Indefinite Padding, further
///   messages after it, and trailing link padding are all handled.
/// - **Without one**, a bundle found before any message has been parsed
///   (at the start of the PDU, or after nothing but Indefinite Padding) is
///   taken to run to the end of the PDU and is yielded as a single
///   [`Message::Bundle`]; one found after a message is a terminal
///   [`Error::EncapsulatedBundle`], and the rest of the PDU is discarded,
///   as Section 7.3 requires of a receiver that cannot find the extent.
///
/// The hookless rule is only correct when the link delivers the frame at
/// exactly the bundle's length.  A link that pads frames to a minimum or
/// fixed size (Ethernet's 46-octet minimum payload, fixed-length CCSDS
/// frames) hands the padding to the consumer as bundle bytes, and a peer
/// that follows a bare bundle with padding or further messages loses them.
/// Such links need the hook; the sender-side counterpart is documented on
/// [`BundleFraming::Bare`](crate::sender::BundleFraming::Bare).  A BTP-U
/// PDU has neither problem: link zero-fill after its last message decodes
/// as Indefinite Padding.
pub fn decode_pdu(pdu: Bytes) -> MessageIter<'static> {
    decode_pdu_with(pdu, DecodeOptions::default())
}

/// Lazily decode the messages in a single convergence layer PDU with
/// explicit [`DecodeOptions`].  See [`decode_pdu`] for the iteration and
/// fault-containment rules.
pub fn decode_pdu_with(pdu: Bytes, options: DecodeOptions<'_>) -> MessageIter<'_> {
    MessageIter {
        pdu,
        offset: 0,
        options,
        parsed_message: false,
        done: false,
    }
}

/// Lazy message iterator over a PDU, returned by [`decode_pdu`] and
/// [`decode_pdu_with`].
///
/// Owns the PDU [`Bytes`], so yielded messages hold zero-copy views into it.
/// An [`Err`] for a message whose extent was known is recoverable: iteration
/// resumes at the next message boundary. An [`Err`] from the framing itself
/// exhausts the iterator: the stream position is unreliable, so no further
/// messages are parsed.
pub struct MessageIter<'a> {
    pdu: Bytes,
    offset: usize,
    options: DecodeOptions<'a>,
    /// Whether a message header has been framed yet.  Until one has, a
    /// bundle-reserved byte with no extent hook is read as a bare bundle
    /// frame running to the end of the PDU.
    parsed_message: bool,
    done: bool,
}

impl fmt::Debug for MessageIter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageIter")
            .field("pdu_len", &self.pdu.len())
            .field("offset", &self.offset)
            .field("options", &self.options)
            .field("parsed_message", &self.parsed_message)
            .field("done", &self.done)
            .finish()
    }
}

impl MessageIter<'_> {
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

    /// Decode the BTP-U message at `offset`.
    ///
    /// Framing errors (a truncated header or a length past the buffer) are
    /// terminal for the whole PDU: the next message boundary cannot be
    /// determined.  Once the extent is known, an interior fault is contained
    /// to this one message: the offset moves to the next boundary the header
    /// length gives and iteration continues.
    fn next_message(&mut self) -> Result<Message> {
        let hdr = match decode_header(&self.pdu[self.offset..]) {
            Ok(hdr) => hdr,
            // Counted from the start of the PDU, as the other framing
            // errors are, rather than from the start of the header.
            Err(Error::InsufficientData { .. }) => {
                self.done = true;
                return Err(Error::InsufficientData {
                    needed: self.offset + HEADER_SIZE,
                    available: self.pdu.len(),
                });
            }
            Err(e) => {
                self.done = true;
                return Err(e);
            }
        };
        // A 20-bit field: lossless on every supported target.
        let content_end = self.offset + HEADER_SIZE + hdr.length as usize;
        if content_end > self.pdu.len() {
            self.done = true;
            return Err(Error::InsufficientData {
                needed: content_end,
                available: self.pdu.len(),
            });
        }
        self.parsed_message = true;
        let content = self.pdu.slice(self.offset + HEADER_SIZE..content_end);
        self.offset = content_end;
        decode_message(hdr, content, self.options)
    }

    /// Deliver the encapsulated bundle at `offset` (Section 7.3), whose
    /// first byte is bundle-reserved.
    ///
    /// The extent comes from the caller's [`BundleExtent`] hook if there is
    /// one; failing that, a bundle that precedes every message is taken to
    /// fill the rest of the PDU (the bare-frame case).  Anything else is
    /// terminal: the extent is unknowable and Section 7.3 forbids processing
    /// the remainder.
    fn next_encapsulated_bundle(&mut self) -> Result<Message> {
        let first_byte = self.pdu[self.offset];
        let remaining = self.pdu.len() - self.offset;
        let extent = match self.options.bundle_extent {
            Some(hook) => hook.bundle_extent(&self.pdu[self.offset..]),
            None if !self.parsed_message => Some(remaining),
            None => None,
        };
        match extent {
            Some(n) if n > 0 && n <= remaining => {
                let data = self.pdu.slice(self.offset..self.offset + n);
                self.offset += n;
                Ok(Message::Bundle {
                    hints: Vec::new(),
                    data,
                })
            }
            Some(n) if n > remaining => {
                self.done = true;
                Err(Error::InsufficientData {
                    needed: self.offset.saturating_add(n),
                    available: self.pdu.len(),
                })
            }
            _ => {
                self.done = true;
                Err(Error::EncapsulatedBundle {
                    first_byte,
                    offset: self.offset,
                })
            }
        }
    }
}

impl Iterator for MessageIter<'_> {
    type Item = Result<Message>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        // Indefinite padding: skip a run of zero bytes (Section 8.6).  It
        // has no semantic content and the receiver MUST ignore it, so no
        // item is yielded for it.
        match self.pdu[self.offset..].iter().position(|&b| b != 0) {
            Some(n) => self.offset += n,
            None => {
                self.offset = self.pdu.len();
                self.done = true;
                return None;
            }
        }

        Some(match frame_kind(&self.pdu[self.offset..]) {
            FrameKind::BtpuPdu => self.next_message(),
            FrameKind::Bpv6Bundle | FrameKind::Bpv7Bundle => self.next_encapsulated_bundle(),
        })
    }
}

impl FusedIterator for MessageIter<'_> {}

/// Decode one message whose header and content have been framed.
fn decode_message(
    hdr: MessageHeader,
    content: Bytes,
    options: DecodeOptions<'_>,
) -> Result<Message> {
    // Resolve the message type BEFORE parsing anything from the content.
    // Section 7.3 requires an unrecognized type to be skipped via the
    // header length field and processing to continue; it is preserved
    // opaquely, its content (hint bytes included) uninterpreted, so a
    // malformed or extension-defined hint chain in an unknown message
    // cannot error the rest of the PDU.  With FEC decoding off, the four
    // provisional FEC codes are Private Use values like any other.
    let known = MessageType::from_byte(hdr.message_type).filter(|mt| options.fec || !mt.is_fec());
    let Some(mt) = known else {
        return Ok(Message::Unknown {
            message_type: hdr.message_type,
            flags: hdr.flags,
            data: content,
        });
    };

    match mt {
        // Receivers MUST ignore Definite Padding content (Section 8.5), so
        // it is never parsed.  Type 0 never arrives here: the iterator
        // consumes zero runs before framing a header, and a zero byte is
        // headerless (Section 8.6).  Were one framed anyway, its declared
        // content could only be padding.
        MessageType::IndefinitePadding | MessageType::DefinitePadding => {
            Ok(Message::DefinitePadding { len: content.len() })
        }

        MessageType::Bundle => {
            let (hints, data) = split_hints(hdr.flags, content)?;
            Ok(Message::Bundle { hints, data })
        }

        MessageType::TransferSegment | MessageType::TransferEnd => {
            let (hints, data) = split_hints(hdr.flags, content)?;
            let (transfer_number, segment_index, data) = decode_transfer_fields(data)?;
            let m = TransferSegmentMessage {
                transfer_number,
                segment_index,
                hints,
                data,
            };
            Ok(if mt == MessageType::TransferEnd {
                Message::TransferEnd(m)
            } else {
                Message::TransferSegment(m)
            })
        }

        MessageType::TransferCancel => {
            let (_, data) = split_hints(hdr.flags, content)?;
            if data.len() < 4 {
                return Err(Error::InsufficientData {
                    needed: 4,
                    available: data.len(),
                });
            }
            let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            Ok(Message::TransferCancel { transfer_number })
        }

        MessageType::PreAgreedFecSource | MessageType::PreAgreedFecRepair => {
            let (hints, data) = split_hints(hdr.flags, content)?;
            let (transfer_number, fec_instance_id, payload) = decode_fec_fields(data)?;
            let m = PreAgreedFecMessage {
                transfer_number,
                fec_instance_id,
                hints,
                payload,
            };
            Ok(if mt == MessageType::PreAgreedFecSource {
                Message::PreAgreedFecSource(m)
            } else {
                Message::PreAgreedFecRepair(m)
            })
        }

        MessageType::ExplicitFecSource | MessageType::ExplicitFecRepair => {
            let (hints, data) = split_hints(hdr.flags, content)?;
            let (transfer_number, fec_encoding_id, payload) = decode_fec_fields(data)?;
            let m = ExplicitFecMessage {
                transfer_number,
                fec_encoding_id,
                hints,
                payload,
            };
            Ok(if mt == MessageType::ExplicitFecSource {
                Message::ExplicitFecSource(m)
            } else {
                Message::ExplicitFecRepair(m)
            })
        }
    }
}

/// Split a message's content into its hint items (if the H flag is set) and
/// the type-specific data that follows them.
fn split_hints(flags: MessageFlags, content: Bytes) -> Result<(Vec<HintItem>, Bytes)> {
    if !flags.hint {
        return Ok((Vec::new(), content));
    }
    let (items, consumed) = decode_hints(&content)?;
    Ok((items, content.slice(consumed..)))
}

/// Parse the common transfer_number (u32) + segment_index (u32) prefix.
fn decode_transfer_fields(data: Bytes) -> Result<(u32, u32, Bytes)> {
    if data.len() < 8 {
        return Err(Error::InsufficientData {
            needed: 8,
            available: data.len(),
        });
    }
    let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let segment_index = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    Ok((transfer_number, segment_index, data.slice(8..)))
}

/// Parse the common transfer_number (u32) + instance/encoding ID (u8) prefix
/// shared by all four FEC messages.  The remaining payload is kept opaque:
/// its scheme-defined internal boundaries are not knowable here (see the
/// struct docs).
fn decode_fec_fields(data: Bytes) -> Result<(u32, u8, Bytes)> {
    if data.len() < 5 {
        return Err(Error::InsufficientData {
            needed: 5,
            available: data.len(),
        });
    }
    let transfer_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let id = data[4];
    Ok((transfer_number, id, data.slice(5..)))
}

/// Returns the total encoded size of a message, exactly matching what
/// [`encode_message`] writes: header + hints + content.
pub fn encoded_message_len(message: &Message) -> usize {
    HEADER_SIZE + message_content_len(message)
}

/// Returns the content length (everything after the 4-byte header).
fn message_content_len(message: &Message) -> usize {
    match message {
        Message::DefinitePadding { len } => *len,
        Message::Bundle { hints, data } => encoded_hints_len(hints) + data.len(),
        Message::TransferSegment(m) | Message::TransferEnd(m) => {
            encoded_hints_len(&m.hints) + 8 + m.data.len()
        }
        Message::TransferCancel { .. } => 4,
        // The four FEC messages share one wire shape: hints, transfer
        // number, ID byte, then the scheme-opaque payload.
        Message::PreAgreedFecSource(PreAgreedFecMessage { hints, payload, .. })
        | Message::PreAgreedFecRepair(PreAgreedFecMessage { hints, payload, .. })
        | Message::ExplicitFecSource(ExplicitFecMessage { hints, payload, .. })
        | Message::ExplicitFecRepair(ExplicitFecMessage { hints, payload, .. }) => {
            encoded_hints_len(hints) + 4 + 1 + payload.len()
        }
        Message::Unknown { data, .. } => data.len(),
    }
}

/// Encode a single message into `dst`.
///
/// Errors leave `dst` untouched: hints are validated and the content length
/// checked before any byte is written.  A [`Message::Unknown`] whose type
/// value is defined by the base protocol or bundle-reserved is refused with
/// [`Error::NotAnUnknownType`]; the four provisional FEC values are Private
/// Use and may be relayed as unknown.
pub fn encode_message(message: &Message, dst: &mut BytesMut) -> Result<()> {
    match message {
        Message::DefinitePadding { len } => {
            write_header(
                MessageType::DefinitePadding.into(),
                MessageFlags::default(),
                *len,
                dst,
            )?;
            dst.put_bytes(0, *len);
        }

        Message::Bundle { hints, data } => {
            validate_hints(hints)?;
            let content_len = encoded_hints_len(hints) + data.len();
            write_header(
                MessageType::Bundle.into(),
                hint_flags(hints),
                content_len,
                dst,
            )?;
            write_hints(hints, dst);
            dst.put_slice(data);
        }

        Message::TransferSegment(m) | Message::TransferEnd(m) => {
            let mt = if matches!(message, Message::TransferEnd(_)) {
                MessageType::TransferEnd
            } else {
                MessageType::TransferSegment
            };
            encode_transfer_message(
                mt.into(),
                m.transfer_number,
                m.segment_index,
                &m.hints,
                &m.data,
                dst,
            )?;
        }

        Message::TransferCancel { transfer_number } => {
            write_header(
                MessageType::TransferCancel.into(),
                MessageFlags::default(),
                4,
                dst,
            )?;
            dst.put_u32(*transfer_number);
        }

        Message::PreAgreedFecSource(m) | Message::PreAgreedFecRepair(m) => {
            let mt = if matches!(message, Message::PreAgreedFecSource(_)) {
                MessageType::PreAgreedFecSource
            } else {
                MessageType::PreAgreedFecRepair
            };
            encode_fec_message(
                mt,
                &m.hints,
                m.transfer_number,
                m.fec_instance_id,
                &m.payload,
                dst,
            )?;
        }

        Message::ExplicitFecSource(m) | Message::ExplicitFecRepair(m) => {
            let mt = if matches!(message, Message::ExplicitFecSource(_)) {
                MessageType::ExplicitFecSource
            } else {
                MessageType::ExplicitFecRepair
            };
            encode_fec_message(
                mt,
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
            if !is_relayable_as_unknown(*message_type) {
                return Err(Error::NotAnUnknownType(*message_type));
            }
            write_header(*message_type, *flags, data.len(), dst)?;
            dst.put_slice(data);
        }
    }
    Ok(())
}

/// Whether a type value may be carried by [`Message::Unknown`]: anything a
/// decoder with FEC off would itself have produced as unknown, which
/// excludes the base-protocol types and the bundle-reserved values.
fn is_relayable_as_unknown(message_type: u8) -> bool {
    frame_kind(&[message_type]) == FrameKind::BtpuPdu
        && !matches!(MessageType::from_byte(message_type), Some(mt) if !mt.is_fec())
}

/// The flags for a locally originated message carrying `hints`.
fn hint_flags(hints: &[HintItem]) -> MessageFlags {
    MessageFlags {
        hint: !hints.is_empty(),
        rfu: 0,
    }
}

fn encode_transfer_message(
    msg_type: u8,
    transfer_number: u32,
    segment_index: u32,
    hints: &[HintItem],
    data: &Bytes,
    dst: &mut BytesMut,
) -> Result<()> {
    validate_hints(hints)?;
    let content_len = encoded_hints_len(hints) + 8 + data.len();
    write_header(msg_type, hint_flags(hints), content_len, dst)?;
    write_hints(hints, dst);
    dst.put_u32(transfer_number);
    dst.put_u32(segment_index);
    dst.put_slice(data);
    Ok(())
}

/// Encode one of the four FEC messages; they share a single wire shape:
/// hints, transfer number, instance/encoding ID byte, then the scheme-opaque
/// payload.
fn encode_fec_message(
    message_type: MessageType,
    hints: &[HintItem],
    transfer_number: u32,
    id: u8,
    payload: &Bytes,
    dst: &mut BytesMut,
) -> Result<()> {
    validate_hints(hints)?;
    let content_len = encoded_hints_len(hints) + 4 + 1 + payload.len();
    write_header(message_type.into(), hint_flags(hints), content_len, dst)?;
    write_hints(hints, dst);
    dst.put_u32(transfer_number);
    dst.put_u8(id);
    dst.put_slice(payload);
    Ok(())
}

/// Write a message header for `length` content bytes, or error with
/// [`Error::LengthOverflow`] (writing nothing) if the 20-bit field cannot
/// hold it.
fn write_header(
    message_type: u8,
    flags: MessageFlags,
    length: usize,
    dst: &mut BytesMut,
) -> Result<()> {
    if length > MAX_CONTENT_LENGTH {
        return Err(Error::LengthOverflow {
            length,
            max: MAX_CONTENT_LENGTH,
        });
    }
    let header = encode_header(&MessageHeader {
        message_type,
        flags,
        length: length as u32,
    })?;
    dst.put_slice(&header);
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
                content_len,
                dst,
            )
            .expect("padding content is capped at MAX_CONTENT_LENGTH");
            dst.put_bytes(0, content_len);
        } else {
            // Indefinite Padding: just zero bytes
            dst.put_bytes(0, remaining);
        }
    }
}
