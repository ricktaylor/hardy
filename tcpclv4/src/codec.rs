use super::*;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::{
    bytes::{Buf, BufMut, Bytes, BytesMut},
    codec::Decoder,
};

/// The TCPCLv4 message codec's failures: bytes that do not decode as the
/// RFC 9174 wire dialect, and the I/O errors of the framed stream.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The message type octet is not one defined by RFC 9174 Section 4.6.
    #[error("Invalid message type {0}")]
    InvalidMessageType(u8),

    /// The framed stream's underlying I/O failed; `From<codec::Error>` for
    /// the session error reclassifies this as transport, not dialect.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A SESS_INIT node ID is not valid UTF-8.
    #[error("Invalid Node Id string: {0}")]
    InvalidNodeIdUtf8(#[from] std::string::FromUtf8Error),

    /// A SESS_INIT node ID does not parse as a BPv7 EID.
    #[error("Invalid Node Id: {0}")]
    InvalidNodeId(#[from] hardy_bpv7::eid::Error),

    /// A session extension item claims more bytes than its message holds.
    #[error("Extension item exceeds remaining length")]
    InvalidExtensionLength,

    /// An XFER_SEGMENT declares a data length beyond the local segment MRU
    /// (RFC 9174 Section 5.2.2), or beyond what this target can address.
    /// The bound is enforced before any buffering: an unbounded declared
    /// length would otherwise grow the frame buffer without limit.
    #[error("XFER_SEGMENT data length {data_length} exceeds the segment MRU {segment_mru}")]
    SegmentTooLarge { data_length: u64, segment_mru: u64 },
}

#[repr(u8)]
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum MessageType {
    XFER_SEGMENT = 1,
    XFER_ACK = 2,
    XFER_REFUSE = 3,
    KEEPALIVE = 4,
    SESS_TERM = 5,
    MSG_REJECT = 6,
    SESS_INIT = 7,
}

impl TryFrom<u8> for MessageType {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::XFER_SEGMENT),
            2 => Ok(Self::XFER_ACK),
            3 => Ok(Self::XFER_REFUSE),
            4 => Ok(Self::KEEPALIVE),
            5 => Ok(Self::SESS_TERM),
            6 => Ok(Self::MSG_REJECT),
            7 => Ok(Self::SESS_INIT),
            n => Err(Error::InvalidMessageType(n)),
        }
    }
}

/*
+-----------------------------+
|       Message Header        |
+-----------------------------+
|   Keepalive Interval (U16)  |
+-----------------------------+
|       Segment MRU (U64)     |
+-----------------------------+
|      Transfer MRU (U64)     |
+-----------------------------+
|     Node ID Length (U16)    |
+-----------------------------+
|    Node ID Data (variable)  |
+-----------------------------+
|      Session Extension      |
|      Items Length (U32)     |
+-----------------------------+
|      Session Extension      |
|         Items (var.)        |
+-----------------------------+ */

#[derive(Debug, Default)]
pub struct SessionInitMessage {
    pub keepalive_interval: u16,
    pub segment_mru: u64,
    pub transfer_mru: u64,
    pub node_id: Option<NodeId>,
    pub session_extensions: Vec<SessionInitExtension>,
}

impl SessionInitMessage {
    fn encode(self, dst: &mut BytesMut) -> Result<(), Error> {
        dst.put_u8(MessageType::SESS_INIT as u8);
        dst.put_u16(self.keepalive_interval);
        dst.put_u64(self.segment_mru);
        dst.put_u64(self.transfer_mru);
        if let Some(node_id) = &self.node_id {
            let node_id_str = node_id.to_string();
            dst.put_u16(node_id_str.len() as u16);
            dst.put(node_id_str.as_bytes());
        } else {
            dst.put_u16(0);
        }
        // RFC 9174 Section 4.6: Session Extension Items Length is the total
        // number of octets used to encode the Session Extension Items list
        let ext_byte_length: u32 = self
            .session_extensions
            .iter()
            .map(|e| 1 + 2 + 2 + e.item_value.len() as u32) // flags(1) + type(2) + length(2) + value
            .sum();
        dst.put_u32(ext_byte_length);
        for extension in self.session_extensions {
            dst.put_u8(extension.flags.into());
            dst.put_u16(extension.item_type);
            dst.put_u16(extension.item_length); // RFC 9174 Section 4.8: U16
            dst.put(extension.item_value);
        }
        Ok(())
    }

    fn decode(src: &mut BytesMut) -> Result<Option<Message>, Error> {
        // Need at least header (1) + keepalive (2) + segment_mru (8) + transfer_mru (8) + node_id_len (2) = 21
        if src.len() < 21 {
            return Ok(None);
        }

        // Skip header byte for parsing, but include it in consumed count
        let mut src_cloned = src.clone();
        src_cloned.advance(1); // Skip message type byte

        let keepalive_interval = src_cloned.get_u16();
        let segment_mru = src_cloned.get_u64();
        let transfer_mru = src_cloned.get_u64();
        let node_id_length = src_cloned.get_u16();
        let node_id = if node_id_length > 0 {
            if src_cloned.len() < node_id_length as usize {
                return Ok(None);
            }
            Some(String::from_utf8(src_cloned.split_to(node_id_length as usize).into())?.parse()?)
        } else {
            None
        };

        if src_cloned.len() < 4 {
            return Ok(None);
        }
        let session_extensions_byte_length = src_cloned.get_u32() as usize;
        // consumed = header (1) + keepalive (2) + segment_mru (8) + transfer_mru (8) +
        //            node_id_length (2) + node_id + ext_items_length (4)
        let mut consumed = 1 + 2 + 8 + 8 + 2 + node_id_length as usize + 4;
        let mut session_extensions = Vec::new();
        // RFC 9174 Section 4.6: parse by remaining byte length, not by count
        let mut ext_remaining = session_extensions_byte_length;
        while ext_remaining > 0 {
            // Minimum extension item: flags(1) + type(2) + length(2) = 5.
            // A residue too short for an item header means the list content
            // disagrees with the declared length, which fails the message
            // (RFC 9174 Section 4.6); skipping it silently would leave the
            // residue in the buffer to desynchronise framing.
            if ext_remaining < 5 {
                return Err(Error::InvalidExtensionLength);
            }
            if src_cloned.len() < 5 {
                return Ok(None);
            }
            let flags = src_cloned.get_u8().into();
            let item_type = src_cloned.get_u16();
            let item_length = src_cloned.get_u16(); // RFC 9174 Section 4.8: U16
            if src_cloned.len() < item_length as usize {
                return Ok(None);
            }
            let ext_size = 5 + item_length as usize;
            session_extensions.push(SessionInitExtension {
                flags,
                item_type,
                item_length,
                item_value: src_cloned.split_to(item_length as usize).into(),
            });
            consumed += ext_size;
            ext_remaining = ext_remaining
                .checked_sub(ext_size)
                .ok_or(Error::InvalidExtensionLength)?;
        }
        src.advance(consumed);
        Ok(Some(Message::SessionInit(SessionInitMessage {
            keepalive_interval,
            segment_mru,
            transfer_mru,
            node_id,
            session_extensions,
        })))
    }
}

/*
                     1 1 1 1 1 1 1 1 1 1 2 2 2 2 2 2 2 2 2 2 3 3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+---------------+---------------+---------------+---------------+
|  Item Flags   |           Item Type           | Item Length...|
+---------------+---------------+---------------+---------------+
| length contd. | Item Value...                                 |
+---------------+---------------+---------------+---------------+ */

#[derive(Debug)]
pub struct SessionInitExtension {
    pub flags: SessionInitExtensionFlags,
    pub item_type: u16,
    pub item_length: u16,
    pub item_value: Bytes,
}

#[derive(Debug, Default)]
pub struct SessionInitExtensionFlags {
    pub critical: bool,
    pub reserved: u8,
}

impl From<u8> for SessionInitExtensionFlags {
    fn from(value: u8) -> Self {
        let mut flags = Self::default();
        if value & 1 != 0 {
            flags.critical = true;
        }

        flags.reserved = value & 0xFE;
        if flags.reserved != 0 {
            debug!(
                "Parsing session initialization extension with reserved flag bits set: {:#x}",
                flags.reserved
            );
        }
        flags
    }
}

impl From<SessionInitExtensionFlags> for u8 {
    fn from(value: SessionInitExtensionFlags) -> u8 {
        // Reserved flag bits are receive-side information only: the sender
        // SHALL transmit them as 0 (RFC 9174 Section 4.8).
        let mut flags = 0;
        if value.critical {
            flags |= 1;
        }
        flags
    }
}

/*
+-----------------------------+
|       Message Header        |
+-----------------------------+
|     Message Flags (U8)      |
+-----------------------------+
|      Reason Code (U8)       |
+-----------------------------+ */

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct SessionTermMessage {
    pub message_flags: SessionTermMessageFlags,
    pub reason_code: SessionTermReasonCode,
}

impl SessionTermMessage {
    fn encode(self, dst: &mut BytesMut) -> Result<(), Error> {
        dst.put_u8(MessageType::SESS_TERM as u8);
        dst.put_u8(self.message_flags.into());
        dst.put_u8(self.reason_code.into());
        Ok(())
    }

    fn decode(src: &mut BytesMut) -> Result<Option<Message>, Error> {
        // header (1) + flags (1) + reason (1) = 3
        if src.len() < 3 {
            Ok(None)
        } else {
            src.advance(1); // Skip message type
            Ok(Some(Message::SessionTerm(SessionTermMessage {
                message_flags: src.get_u8().into(),
                reason_code: src.get_u8().into(),
            })))
        }
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct SessionTermMessageFlags {
    pub reply: bool,
    pub reserved: u8,
}

impl From<u8> for SessionTermMessageFlags {
    fn from(value: u8) -> Self {
        let mut flags = Self::default();
        if value & 1 != 0 {
            flags.reply = true;
        }

        flags.reserved = value & 0xFE;
        if flags.reserved != 0 {
            debug!(
                "Parsing session term message with reserved flag bits set: {:#x}",
                flags.reserved
            );
        }
        flags
    }
}

impl From<SessionTermMessageFlags> for u8 {
    fn from(value: SessionTermMessageFlags) -> u8 {
        // Reserved flag bits are receive-side information only: the sender
        // SHALL transmit them as 0 (RFC 9174 Section 6.1).
        let mut flags = 0;
        if value.reply {
            flags |= 1;
        }
        flags
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub enum SessionTermReasonCode {
    #[default]
    Unknown,
    IdleTimeout,
    VersionMismatch,
    Busy,
    ContactFailure,
    ResourceExhaustion,
    Unassigned(u8),
    Private(u8),
}

impl From<u8> for SessionTermReasonCode {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Unknown,
            1 => Self::IdleTimeout,
            2 => Self::VersionMismatch,
            3 => Self::Busy,
            4 => Self::ContactFailure,
            5 => Self::ResourceExhaustion,
            n @ 6..=0xEF => {
                debug!("Parsing session term message with unassigned reason code: {n}");
                Self::Unassigned(n)
            }
            n @ 0xF0..=0xFF => {
                debug!("Parsing session term message with private reason code: {n}");
                Self::Private(n)
            }
        }
    }
}

impl From<SessionTermReasonCode> for u8 {
    fn from(value: SessionTermReasonCode) -> u8 {
        match value {
            SessionTermReasonCode::Unknown => 0,
            SessionTermReasonCode::IdleTimeout => 1,
            SessionTermReasonCode::VersionMismatch => 2,
            SessionTermReasonCode::Busy => 3,
            SessionTermReasonCode::ContactFailure => 4,
            SessionTermReasonCode::ResourceExhaustion => 5,
            SessionTermReasonCode::Unassigned(n) | SessionTermReasonCode::Private(n) => n,
        }
    }
}

/*
+-----------------------------+
|       Message Header        |
+-----------------------------+
|      Reason Code (U8)       |
+-----------------------------+
|   Rejected Message Header   |
+-----------------------------+ */

#[derive(Debug)]
pub struct MessageRejectMessage {
    pub reason_code: MessageRejectionReasonCode,
    pub rejected_message: u8,
}

impl MessageRejectMessage {
    fn encode(self, dst: &mut BytesMut) -> Result<(), Error> {
        dst.put_u8(MessageType::MSG_REJECT as u8);
        dst.put_u8(self.reason_code.into());
        dst.put_u8(self.rejected_message);
        Ok(())
    }

    fn decode(src: &mut BytesMut) -> Result<Option<Message>, Error> {
        // header (1) + reason (1) + rejected_message (1) = 3
        if src.len() < 3 {
            Ok(None)
        } else {
            src.advance(1); // Skip message type
            Ok(Some(Message::Reject(MessageRejectMessage {
                reason_code: src.get_u8().into(),
                // Ensure we convert the rejected message type to something we could have sent!
                rejected_message: codec::MessageType::try_from(src.get_u8())? as u8,
            })))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRejectionReasonCode {
    UnknownType,
    Unsupported,
    Unexpected,
    Unassigned(u8),
    Private(u8),
}

impl From<u8> for MessageRejectionReasonCode {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::UnknownType,
            2 => Self::Unsupported,
            3 => Self::Unexpected,
            n @ 0 | n @ 4..=0xEF => {
                debug!("Parsing rejection message with unassigned reason code: {n}");
                Self::Unassigned(n)
            }
            n @ 0xF0..=0xFF => {
                debug!("Parsing rejection message with private reason code: {n}");
                Self::Private(n)
            }
        }
    }
}

impl From<MessageRejectionReasonCode> for u8 {
    fn from(value: MessageRejectionReasonCode) -> u8 {
        match value {
            MessageRejectionReasonCode::UnknownType => 1,
            MessageRejectionReasonCode::Unsupported => 2,
            MessageRejectionReasonCode::Unexpected => 3,
            MessageRejectionReasonCode::Unassigned(n) | MessageRejectionReasonCode::Private(n) => n,
        }
    }
}

/*
+-----------------------------+
|       Message Header        |
+-----------------------------+
|      Reason Code (U8)       |
+-----------------------------+
|      Transfer ID (U64)      |
+-----------------------------+ */

#[derive(Debug)]
pub struct TransferRefuseMessage {
    pub reason_code: TransferRefuseReasonCode,
    pub transfer_id: u64,
}

impl TransferRefuseMessage {
    fn encode(self, dst: &mut BytesMut) -> Result<(), Error> {
        dst.put_u8(MessageType::XFER_REFUSE as u8);
        dst.put_u8(self.reason_code.into());
        dst.put_u64(self.transfer_id);
        Ok(())
    }

    fn decode(src: &mut BytesMut) -> Result<Option<Message>, Error> {
        // header (1) + reason (1) + transfer_id (8) = 10
        if src.len() < 10 {
            Ok(None)
        } else {
            src.advance(1); // Skip message type
            Ok(Some(Message::TransferRefuse(TransferRefuseMessage {
                reason_code: src.get_u8().into(),
                transfer_id: src.get_u64(),
            })))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TransferRefuseReasonCode {
    Unknown,
    Completed,
    NoResources,
    Retransmit,
    NotAcceptable,
    ExtensionFailure,
    SessionTerminating,
    Unassigned(u8),
    Private(u8),
}

impl From<u8> for TransferRefuseReasonCode {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Unknown,
            1 => Self::Completed,
            2 => Self::NoResources,
            3 => Self::Retransmit,
            4 => Self::NotAcceptable,
            5 => Self::ExtensionFailure,
            6 => Self::SessionTerminating,
            n @ 7..=0xEF => {
                debug!("Parsing transfer refuse message with unassigned reason code: {n}");
                Self::Unassigned(n)
            }
            n @ 0xF0..=0xFF => {
                debug!("Parsing transfer refuse message with private reason code: {n}");
                Self::Private(n)
            }
        }
    }
}

impl From<TransferRefuseReasonCode> for u8 {
    fn from(value: TransferRefuseReasonCode) -> u8 {
        match value {
            TransferRefuseReasonCode::Unknown => 0,
            TransferRefuseReasonCode::Completed => 1,
            TransferRefuseReasonCode::NoResources => 2,
            TransferRefuseReasonCode::Retransmit => 3,
            TransferRefuseReasonCode::NotAcceptable => 4,
            TransferRefuseReasonCode::ExtensionFailure => 5,
            TransferRefuseReasonCode::SessionTerminating => 6,
            TransferRefuseReasonCode::Unassigned(n) | TransferRefuseReasonCode::Private(n) => n,
        }
    }
}

/*
+-----------------------------+
|       Message Header        |
+-----------------------------+
|     Message Flags (U8)      |
+-----------------------------+
|      Transfer ID (U64)      |
+-----------------------------+
| Acknowledged length (U64)   |
+-----------------------------+ */

#[derive(Debug)]
pub struct TransferAckMessage {
    pub message_flags: TransferSegmentMessageFlags,
    pub transfer_id: u64,
    pub acknowledged_length: u64,
}

impl TransferAckMessage {
    fn encode(self, dst: &mut BytesMut) -> Result<(), Error> {
        dst.put_u8(MessageType::XFER_ACK as u8);
        dst.put_u8(self.message_flags.into());
        dst.put_u64(self.transfer_id);
        dst.put_u64(self.acknowledged_length);
        Ok(())
    }

    fn decode(src: &mut BytesMut) -> Result<Option<Message>, Error> {
        // header (1) + flags (1) + transfer_id (8) + acknowledged_length (8) = 18
        if src.len() < 18 {
            Ok(None)
        } else {
            src.advance(1); // Skip message type
            Ok(Some(Message::TransferAck(TransferAckMessage {
                message_flags: src.get_u8().into(),
                transfer_id: src.get_u64(),
                acknowledged_length: src.get_u64(),
            })))
        }
    }
}

#[derive(Debug)]
pub enum Message {
    SessionInit(SessionInitMessage),
    SessionTerm(SessionTermMessage),
    Keepalive,
    TransferSegment(TransferSegmentMessage),
    TransferAck(TransferAckMessage),
    TransferRefuse(TransferRefuseMessage),
    Reject(MessageRejectMessage),
}

impl Message {
    pub fn message_type(&self) -> MessageType {
        match self {
            Message::SessionInit(_) => MessageType::SESS_INIT,
            Message::SessionTerm(_) => MessageType::SESS_TERM,
            Message::Keepalive => MessageType::KEEPALIVE,
            Message::TransferSegment(_) => MessageType::XFER_SEGMENT,
            Message::TransferAck(_) => MessageType::XFER_ACK,
            Message::TransferRefuse(_) => MessageType::XFER_REFUSE,
            Message::Reject(_) => MessageType::MSG_REJECT,
        }
    }
}

/*
+------------------------------+
|       Message Header         |
+------------------------------+
|     Message Flags (U8)       |
+------------------------------+
|      Transfer ID (U64)       |
+------------------------------+
|     Transfer Extension       |
|      Items Length (U32)      |
|   (only for START segment)   |
+------------------------------+
|     Transfer Extension       |
|         Items (var.)         |
|   (only for START segment)   |
+------------------------------+
|      Data length (U64)       |
+------------------------------+
| Data contents (octet string) |
+------------------------------+ */

#[derive(Debug, Default)]
pub struct TransferSegmentMessage {
    pub message_flags: TransferSegmentMessageFlags,
    pub transfer_id: u64,
    pub transfer_extensions: Vec<TransferSegmentExtension>,
    pub data: Bytes,
}

impl TransferSegmentMessage {
    fn encode(self, dst: &mut BytesMut) -> Result<(), Error> {
        dst.put_u8(MessageType::XFER_SEGMENT as u8);
        dst.put_u8(self.message_flags.clone().into());
        dst.put_u64(self.transfer_id);
        if self.message_flags.start {
            // RFC 9174 Section 5.2.2: Transfer Extension Items Length is the total
            // number of octets used to encode the Transfer Extension Items list
            let ext_byte_length: u32 = self
                .transfer_extensions
                .iter()
                .map(|e| 1 + 2 + 2 + e.item_value.len() as u32) // flags(1) + type(2) + length(2) + value
                .sum();
            dst.put_u32(ext_byte_length);
            for extension in self.transfer_extensions {
                dst.put_u8(extension.flags.into());
                dst.put_u16(extension.item_type);
                dst.put_u16(extension.item_length); // RFC 9174 Section 5.2.5: U16
                dst.put(extension.item_value);
            }
        }
        dst.put_u64(self.data.len() as u64);
        dst.put(self.data);
        Ok(())
    }

    fn decode(src: &mut BytesMut, segment_mru: u64) -> Result<Option<Message>, Error> {
        // header (1) + flags (1) + transfer_id (8) = 10 minimum
        if src.len() < 10 {
            return Ok(None);
        }
        let mut src_cloned = src.clone();
        src_cloned.advance(1); // Skip message type byte
        let message_flags: TransferSegmentMessageFlags = src_cloned.get_u8().into();
        let transfer_id = src_cloned.get_u64();

        // consumed includes header byte
        let mut consumed: usize = 10;
        let mut transfer_extensions = Vec::new();
        if message_flags.start {
            if src_cloned.len() < 4 {
                return Ok(None);
            }
            let transfer_extensions_byte_length = src_cloned.get_u32() as usize;
            consumed += 4;
            // RFC 9174 Section 5.2.2: parse by remaining byte length, not by count
            let mut ext_remaining = transfer_extensions_byte_length;
            while ext_remaining > 0 {
                // Minimum extension item: flags(1) + type(2) + length(2) = 5.
                // A residue too short for an item header means the list
                // content disagrees with the declared length, which fails
                // the message (RFC 9174 Section 5.2.2); skipping it silently
                // would leave the residue in the buffer to desynchronise
                // framing.
                if ext_remaining < 5 {
                    return Err(Error::InvalidExtensionLength);
                }
                if src_cloned.len() < 5 {
                    return Ok(None);
                }
                let flags = src_cloned.get_u8().into();
                let item_type = src_cloned.get_u16();
                let item_length = src_cloned.get_u16(); // RFC 9174 Section 5.2.5: U16
                let ext_size = 5 + item_length as usize;
                if src_cloned.len() < item_length as usize {
                    return Ok(None);
                }
                transfer_extensions.push(TransferSegmentExtension {
                    flags,
                    item_type,
                    item_length,
                    item_value: src_cloned.split_to(item_length as usize).into(),
                });
                consumed += ext_size;
                ext_remaining = ext_remaining
                    .checked_sub(ext_size)
                    .ok_or(Error::InvalidExtensionLength)?;
            }
        }
        if src_cloned.len() < 8 {
            return Ok(None);
        }
        let data_length = src_cloned.get_u64();
        // Bound the declared length before any buffering decision: a peer
        // never sends a segment beyond our advertised segment MRU (RFC 9174
        // Section 5.2.2), and waiting for an unbounded declared length would
        // grow the frame buffer without limit. The comparison stays in u64,
        // and the conversion below rejects lengths this target cannot
        // address, so a 32-bit build cannot truncate the length and
        // desynchronise framing.
        if data_length > segment_mru {
            return Err(Error::SegmentTooLarge {
                data_length,
                segment_mru,
            });
        }
        let data_length = usize::try_from(data_length).map_err(|_| Error::SegmentTooLarge {
            data_length,
            segment_mru,
        })?;
        if src_cloned.len() < data_length {
            return Ok(None);
        }
        // Skip the header bytes (message type, flags, transfer_id, extensions, data_length)
        let _ = src.split_to(consumed + 8);
        Ok(Some(Message::TransferSegment(TransferSegmentMessage {
            message_flags,
            transfer_id,
            transfer_extensions,
            data: src.split_to(data_length).into(),
        })))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TransferSegmentMessageFlags {
    pub start: bool,
    pub end: bool,
    pub reserved: u8,
}

impl From<u8> for TransferSegmentMessageFlags {
    fn from(value: u8) -> Self {
        let mut flags = Self::default();
        if value & 1 != 0 {
            flags.end = true;
        }
        if value & 2 != 0 {
            flags.start = true;
        }

        flags.reserved = value & 0xFC;
        if flags.reserved != 0 {
            debug!(
                "Parsing transfer segment message with reserved flag bits set: {:#x}",
                flags.reserved
            );
        }
        flags
    }
}

impl From<TransferSegmentMessageFlags> for u8 {
    fn from(value: TransferSegmentMessageFlags) -> u8 {
        // Reserved flag bits are receive-side information only: the sender
        // SHALL transmit them as 0 (RFC 9174 Section 5.2.2).
        let mut flags = 0;
        if value.end {
            flags |= 1;
        }
        if value.start {
            flags |= 2;
        }
        flags
    }
}

/*
                     1 1 1 1 1 1 1 1 1 1 2 2 2 2 2 2 2 2 2 2 3 3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+---------------+---------------+---------------+---------------+
|  Item Flags   |           Item Type           | Item Length...|
+---------------+---------------+---------------+---------------+
| length contd. | Item Value...                                 |
+---------------+---------------+---------------+---------------+ */

#[derive(Debug)]
pub struct TransferSegmentExtension {
    pub flags: TransferSegmentExtensionFlags,
    pub item_type: u16,
    pub item_length: u16,
    pub item_value: Bytes,
}

#[derive(Debug, Default)]
pub struct TransferSegmentExtensionFlags {
    pub critical: bool,
    pub reserved: u8,
}

impl From<u8> for TransferSegmentExtensionFlags {
    fn from(value: u8) -> Self {
        let mut flags = Self::default();
        if value & 1 != 0 {
            flags.critical = true;
        }

        flags.reserved = value & 0xFE;
        if flags.reserved != 0 {
            debug!(
                "Parsing transfer segment extension with reserved flag bits set: {:#x}",
                flags.reserved
            );
        }
        flags
    }
}

impl From<TransferSegmentExtensionFlags> for u8 {
    fn from(value: TransferSegmentExtensionFlags) -> u8 {
        // Reserved flag bits are receive-side information only: the sender
        // SHALL transmit them as 0 (RFC 9174 Section 5.2.5).
        let mut flags = 0;
        if value.critical {
            flags |= 1;
        }
        flags
    }
}

pub struct MessageCodec {
    // The local segment MRU: the largest data length an inbound
    // XFER_SEGMENT may declare (RFC 9174 Section 5.2.2). Enforced in the
    // decoder so a hostile declared length is rejected before the framed
    // buffer commits to it.
    segment_mru: u64,
}

impl MessageCodec {
    pub fn new_framed<T: AsyncRead + AsyncWrite + Sized>(
        io: T,
        segment_mru: u64,
    ) -> tokio_util::codec::Framed<T, Self> {
        Self { segment_mru }.framed(io)
    }
}

impl tokio_util::codec::Decoder for MessageCodec {
    type Item = Message;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.is_empty() {
            // Not enough data to read message header
            return Ok(None);
        }

        let before = src.len();

        // Peek at message type without consuming it - sub-decoders will handle
        // consuming the header byte only when the full message is available
        let result = match src[0].try_into()? {
            MessageType::XFER_SEGMENT => TransferSegmentMessage::decode(src, self.segment_mru),
            MessageType::XFER_ACK => TransferAckMessage::decode(src),
            MessageType::XFER_REFUSE => TransferRefuseMessage::decode(src),
            MessageType::KEEPALIVE => {
                // KEEPALIVE is just the message type byte, consume it
                src.advance(1);
                Ok(Some(Message::Keepalive))
            }
            MessageType::SESS_TERM => SessionTermMessage::decode(src),
            MessageType::MSG_REJECT => MessageRejectMessage::decode(src),
            MessageType::SESS_INIT => SessionInitMessage::decode(src),
        };

        if let Ok(Some(_)) = &result {
            metrics::counter!("tcpclv4.session.bytes.received")
                .increment((before - src.len()) as u64);
        }

        result
    }
}

impl tokio_util::codec::Encoder<Message> for MessageCodec {
    type Error = Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let before = dst.len();
        let result = match item {
            Message::SessionInit(m) => m.encode(dst),
            Message::SessionTerm(m) => m.encode(dst),
            Message::Keepalive => {
                dst.put_u8(MessageType::KEEPALIVE as u8);
                Ok(())
            }
            Message::TransferSegment(m) => m.encode(dst),
            Message::TransferAck(m) => m.encode(dst),
            Message::TransferRefuse(m) => m.encode(dst),
            Message::Reject(m) => m.encode(dst),
        };
        if result.is_ok() {
            metrics::counter!("tcpclv4.session.bytes.sent").increment((dst.len() - before) as u64);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::codec::{Decoder, Encoder};

    // Large enough that no serdes payload trips the inbound segment bound,
    // small enough that a test can exceed it without allocating.
    const TEST_SEGMENT_MRU: u64 = 1 << 20;

    fn test_codec() -> MessageCodec {
        MessageCodec {
            segment_mru: TEST_SEGMENT_MRU,
        }
    }

    fn encode_msg(msg: Message) -> BytesMut {
        let mut codec = test_codec();
        let mut buf = BytesMut::new();
        codec.encode(msg, &mut buf).unwrap();
        buf
    }

    fn decode_msg(buf: &[u8]) -> Option<Message> {
        let mut codec = test_codec();
        let mut src = BytesMut::from(buf);
        let msg = codec.decode(&mut src).unwrap();
        if msg.is_some() {
            // A decoded message must consume exactly its encoded bytes:
            // leftover or over-consumed bytes desynchronise framing of the
            // next message on a real stream.
            assert!(
                src.is_empty(),
                "decoder left {} unconsumed bytes",
                src.len()
            );
        }
        msg
    }

    // ---- UT-TCP-01: Message SerDes ----

    // UT-TCP-01: KEEPALIVE round-trip (simplest message — single byte).
    #[test]
    fn serdes_keepalive() {
        let buf = encode_msg(Message::Keepalive);
        assert_eq!(&buf[..], &[MessageType::KEEPALIVE as u8]);

        let msg = decode_msg(&buf).unwrap();
        assert!(matches!(msg, Message::Keepalive));
    }

    // UT-TCP-01: SESS_TERM round-trip with reason code and reply flag.
    #[test]
    fn serdes_sess_term() {
        let original = SessionTermMessage {
            message_flags: SessionTermMessageFlags {
                reply: true,
                reserved: 0,
            },
            reason_code: SessionTermReasonCode::IdleTimeout,
        };

        let buf = encode_msg(Message::SessionTerm(original.clone()));
        assert_eq!(buf[0], MessageType::SESS_TERM as u8);
        assert_eq!(buf.len(), 3); // header + flags + reason

        let Message::SessionTerm(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected SessionTerm")
        };
        assert!(decoded.message_flags.reply);
        assert_eq!(decoded.reason_code, SessionTermReasonCode::IdleTimeout);
    }

    // UT-TCP-01: SESS_INIT round-trip with node ID and no extensions.
    #[test]
    fn serdes_sess_init_basic() {
        let original = SessionInitMessage {
            keepalive_interval: 60,
            segment_mru: 16384,
            transfer_mru: 0x4000_0000,
            node_id: Some("ipn:1.0".parse().unwrap()),
            session_extensions: vec![],
        };

        let buf = encode_msg(Message::SessionInit(original));
        assert_eq!(buf[0], MessageType::SESS_INIT as u8);

        let Message::SessionInit(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected SessionInit")
        };
        assert_eq!(decoded.keepalive_interval, 60);
        assert_eq!(decoded.segment_mru, 16384);
        assert_eq!(decoded.transfer_mru, 0x4000_0000);
        assert_eq!(decoded.node_id.unwrap().to_string(), "ipn:1.0");
        assert!(decoded.session_extensions.is_empty());
    }

    // UT-TCP-01: SESS_INIT round-trip with no node ID.
    #[test]
    fn serdes_sess_init_no_node_id() {
        let original = SessionInitMessage {
            keepalive_interval: 0,
            segment_mru: 1024,
            transfer_mru: 1024,
            node_id: None,
            session_extensions: vec![],
        };

        let buf = encode_msg(Message::SessionInit(original));
        let Message::SessionInit(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected SessionInit")
        };
        assert!(decoded.node_id.is_none());
        assert_eq!(decoded.keepalive_interval, 0);
    }

    // UT-TCP-01: SESS_INIT round-trip with a session extension item.
    #[test]
    fn serdes_sess_init_with_extension() {
        let ext_data = Bytes::from_static(b"\x01\x02\x03\x04");
        let original = SessionInitMessage {
            keepalive_interval: 30,
            segment_mru: 8192,
            transfer_mru: 65536,
            node_id: Some("ipn:2.0".parse().unwrap()),
            session_extensions: vec![SessionInitExtension {
                flags: SessionInitExtensionFlags {
                    critical: true,
                    reserved: 0,
                },
                item_type: 0x00FF,
                item_length: 4,
                item_value: ext_data.clone(),
            }],
        };

        let buf = encode_msg(Message::SessionInit(original));
        let Message::SessionInit(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected SessionInit")
        };
        assert_eq!(decoded.session_extensions.len(), 1);
        let ext = &decoded.session_extensions[0];
        assert!(ext.flags.critical);
        assert_eq!(ext.item_type, 0x00FF);
        assert_eq!(ext.item_length, 4);
        assert_eq!(&ext.item_value[..], &ext_data[..]);
    }

    // UT-TCP-01: XFER_SEGMENT round-trip (START+END, single segment).
    #[test]
    fn serdes_xfer_segment() {
        let payload = Bytes::from_static(b"hello bundle");
        let original = TransferSegmentMessage {
            message_flags: TransferSegmentMessageFlags {
                start: true,
                end: true,
                reserved: 0,
            },
            transfer_id: 42,
            transfer_extensions: vec![],
            data: payload.clone(),
        };

        let buf = encode_msg(Message::TransferSegment(original));
        assert_eq!(buf[0], MessageType::XFER_SEGMENT as u8);

        let Message::TransferSegment(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected TransferSegment")
        };
        assert!(decoded.message_flags.start);
        assert!(decoded.message_flags.end);
        assert_eq!(decoded.transfer_id, 42);
        assert_eq!(&decoded.data[..], b"hello bundle");
    }

    // UT-TCP-01: XFER_ACK round-trip.
    #[test]
    fn serdes_xfer_ack() {
        let buf = encode_msg(Message::TransferAck(TransferAckMessage {
            message_flags: TransferSegmentMessageFlags {
                start: false,
                end: true,
                reserved: 0,
            },
            transfer_id: 99,
            acknowledged_length: 1000,
        }));
        assert_eq!(buf.len(), 18); // header(1) + flags(1) + id(8) + length(8)

        let Message::TransferAck(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected TransferAck")
        };
        assert_eq!(decoded.transfer_id, 99);
        assert_eq!(decoded.acknowledged_length, 1000);
        assert!(decoded.message_flags.end);
    }

    // UT-TCP-01: XFER_REFUSE round-trip.
    #[test]
    fn serdes_xfer_refuse() {
        let buf = encode_msg(Message::TransferRefuse(TransferRefuseMessage {
            reason_code: TransferRefuseReasonCode::NoResources,
            transfer_id: 7,
        }));
        assert_eq!(buf.len(), 10); // header(1) + reason(1) + id(8)

        let Message::TransferRefuse(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected TransferRefuse")
        };
        assert_eq!(decoded.transfer_id, 7);
        assert_eq!(decoded.reason_code, TransferRefuseReasonCode::NoResources);
    }

    // UT-TCP-01: Invalid message type byte returns error.
    #[test]
    fn serdes_invalid_message_type() {
        let mut codec = test_codec();
        let mut src = BytesMut::from(&[0xFF_u8][..]);
        let result = codec.decode(&mut src);
        assert!(result.is_err());
    }

    // UT-TCP-01: Messages encoded back-to-back into one buffer decode in
    // sequence, pinning the hand-computed consumed counts of the
    // variable-length decoders: an off-by-N there corrupts the framing of
    // the next message on the stream.
    #[test]
    fn serdes_back_to_back_messages() {
        let mut codec = test_codec();
        let mut buf = BytesMut::new();
        codec
            .encode(
                Message::SessionInit(SessionInitMessage {
                    keepalive_interval: 15,
                    segment_mru: 4096,
                    transfer_mru: 8192,
                    node_id: Some("ipn:3.0".parse().unwrap()),
                    session_extensions: vec![SessionInitExtension {
                        flags: SessionInitExtensionFlags::default(),
                        item_type: 0x0001,
                        item_length: 2,
                        item_value: Bytes::from_static(b"\xAA\xBB"),
                    }],
                }),
                &mut buf,
            )
            .unwrap();
        codec
            .encode(
                Message::TransferSegment(TransferSegmentMessage {
                    message_flags: TransferSegmentMessageFlags {
                        start: true,
                        end: true,
                        reserved: 0,
                    },
                    transfer_id: 11,
                    transfer_extensions: vec![],
                    data: Bytes::from_static(b"payload"),
                }),
                &mut buf,
            )
            .unwrap();
        codec.encode(Message::Keepalive, &mut buf).unwrap();

        let Some(Message::SessionInit(first)) = codec.decode(&mut buf).unwrap() else {
            panic!("expected SessionInit")
        };
        assert_eq!(first.node_id.unwrap().to_string(), "ipn:3.0");
        assert_eq!(first.session_extensions.len(), 1);

        let Some(Message::TransferSegment(second)) = codec.decode(&mut buf).unwrap() else {
            panic!("expected TransferSegment")
        };
        assert_eq!(second.transfer_id, 11);
        assert_eq!(&second.data[..], b"payload");

        let Some(Message::Keepalive) = codec.decode(&mut buf).unwrap() else {
            panic!("expected Keepalive")
        };
        assert!(buf.is_empty());
    }

    // UT-TCP-01: Incomplete message returns None (needs more data).
    #[test]
    fn serdes_incomplete_returns_none() {
        // SESS_TERM needs 3 bytes, give it 2
        let msg = decode_msg(&[MessageType::SESS_TERM as u8, 0x00]);
        assert!(msg.is_none());
    }

    // UT-TCP-01: Every proper prefix of every message decodes to None
    // without consuming bytes. A fragmented TCP read routinely delivers such
    // prefixes, so an off-by-one in any decoder's fixed-size length guard
    // panics the buffer reads on a short frame; this pins each guard at its
    // exact boundary.
    #[test]
    fn every_prefix_of_each_message_returns_none() {
        let corpus = [
            Message::SessionInit(SessionInitMessage {
                keepalive_interval: 15,
                segment_mru: 4096,
                transfer_mru: 8192,
                node_id: Some("ipn:3.0".parse().unwrap()),
                session_extensions: vec![SessionInitExtension {
                    flags: SessionInitExtensionFlags::default(),
                    item_type: 0x0001,
                    item_length: 2,
                    item_value: Bytes::from_static(b"\xAA\xBB"),
                }],
            }),
            Message::SessionTerm(SessionTermMessage::default()),
            Message::TransferSegment(TransferSegmentMessage {
                message_flags: TransferSegmentMessageFlags {
                    start: true,
                    end: true,
                    reserved: 0,
                },
                transfer_id: 11,
                transfer_extensions: vec![TransferSegmentExtension {
                    flags: TransferSegmentExtensionFlags::default(),
                    item_type: 0x0001,
                    item_length: 8,
                    item_value: Bytes::from_static(&[0; 8]),
                }],
                data: Bytes::from_static(b"payload"),
            }),
            Message::TransferAck(TransferAckMessage {
                message_flags: TransferSegmentMessageFlags::default(),
                transfer_id: 99,
                acknowledged_length: 1000,
            }),
            Message::TransferRefuse(TransferRefuseMessage {
                reason_code: TransferRefuseReasonCode::NoResources,
                transfer_id: 7,
            }),
            Message::Reject(MessageRejectMessage {
                reason_code: MessageRejectionReasonCode::Unexpected,
                rejected_message: MessageType::KEEPALIVE as u8,
            }),
        ];

        for msg in corpus {
            let name = format!("{:?}", msg.message_type());
            let buf = encode_msg(msg);
            for prefix_len in 0..buf.len() {
                let mut src = BytesMut::from(&buf[..prefix_len]);
                let decoded = test_codec().decode(&mut src).unwrap_or_else(|e| {
                    panic!("{name}: prefix of {prefix_len} bytes errored: {e}")
                });
                assert!(
                    decoded.is_none(),
                    "{name}: prefix of {prefix_len} bytes decoded to a message"
                );
                assert_eq!(
                    src.len(),
                    prefix_len,
                    "{name}: prefix of {prefix_len} bytes was partially consumed"
                );
            }
        }
    }

    // ---- Malformed extension lists ----

    // The SESS_INIT prelude: header, MRUs, and an empty node ID; the caller
    // appends the extension list length and items.
    fn sess_init_prelude() -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u8(MessageType::SESS_INIT as u8);
        buf.put_u16(0); // keepalive interval
        buf.put_u64(1024); // segment MRU
        buf.put_u64(1024); // transfer MRU
        buf.put_u16(0); // node ID length
        buf
    }

    // An item claiming more octets than the declared list length holds
    // fails the message (RFC 9174 Section 4.6) rather than silently
    // over-consuming past the declared list.
    #[test]
    fn sess_init_extension_overrunning_declared_length_errors() {
        let mut buf = sess_init_prelude();
        buf.put_u32(5); // declared list length: one header-only item
        buf.put_u8(0); // item flags
        buf.put_u16(0x00FF); // item type
        buf.put_u16(200); // item length overruns the declared list
        buf.put_bytes(0, 200);

        assert!(matches!(
            test_codec().decode(&mut buf),
            Err(Error::InvalidExtensionLength)
        ));
    }

    // A declared list length too short for even one item header fails the
    // message; consuming the message without the residue would leave the
    // residue bytes to be misparsed as the next message header.
    #[test]
    fn sess_init_trailing_extension_residue_is_rejected() {
        let mut buf = sess_init_prelude();
        buf.put_u32(3); // declared list length below the 5-byte item header
        buf.put_bytes(0xAA, 3);
        buf.put_u8(MessageType::KEEPALIVE as u8); // next message on the stream

        assert!(matches!(
            test_codec().decode(&mut buf),
            Err(Error::InvalidExtensionLength)
        ));
    }

    // A zero-length item value is legal (RFC 9174 Section 4.8): the item is
    // exactly its 5-byte header, and the list length of exactly 5 pins the
    // loop's boundary.
    #[test]
    fn sess_init_zero_length_extension_round_trips() {
        let buf = encode_msg(Message::SessionInit(SessionInitMessage {
            keepalive_interval: 0,
            segment_mru: 1024,
            transfer_mru: 1024,
            node_id: None,
            session_extensions: vec![SessionInitExtension {
                flags: SessionInitExtensionFlags::default(),
                item_type: 0x00AB,
                item_length: 0,
                item_value: Bytes::new(),
            }],
        }));

        let Message::SessionInit(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected SessionInit")
        };
        assert_eq!(decoded.session_extensions.len(), 1);
        assert_eq!(decoded.session_extensions[0].item_type, 0x00AB);
        assert_eq!(decoded.session_extensions[0].item_length, 0);
        assert!(decoded.session_extensions[0].item_value.is_empty());
    }

    // A node ID that is not UTF-8 fails the message.
    #[test]
    fn sess_init_non_utf8_node_id_errors() {
        let mut buf = BytesMut::new();
        buf.put_u8(MessageType::SESS_INIT as u8);
        buf.put_u16(0); // keepalive interval
        buf.put_u64(1024); // segment MRU
        buf.put_u64(1024); // transfer MRU
        buf.put_u16(2); // node ID length
        buf.put_slice(&[0xFF, 0xFE]); // not UTF-8
        buf.put_u32(0); // extension list length

        assert!(matches!(
            test_codec().decode(&mut buf),
            Err(Error::InvalidNodeIdUtf8(_))
        ));
    }

    // ---- Transfer extensions ----

    // UT-TCP-01: XFER_SEGMENT round-trip carrying a transfer extension item
    // (the shape of RFC 9174 Section 5.2.5's TRANSFER_LENGTH on a START
    // segment), then back-to-back with a KEEPALIVE to pin the extension
    // path's consumed accounting: an off-by-N there desynchronises framing
    // against interoperating peers.
    #[test]
    fn serdes_xfer_segment_with_transfer_extension() {
        let ext_value = Bytes::from_static(&[0, 0, 0, 0, 0, 0, 0x30, 0x39]);
        let segment = || TransferSegmentMessage {
            message_flags: TransferSegmentMessageFlags {
                start: true,
                end: true,
                reserved: 0,
            },
            transfer_id: 42,
            transfer_extensions: vec![TransferSegmentExtension {
                flags: TransferSegmentExtensionFlags {
                    critical: false,
                    reserved: 0,
                },
                item_type: 0x0001,
                item_length: 8,
                item_value: ext_value.clone(),
            }],
            data: Bytes::from_static(b"hello bundle"),
        };

        let buf = encode_msg(Message::TransferSegment(segment()));
        let Message::TransferSegment(decoded) = decode_msg(&buf).unwrap() else {
            panic!("expected TransferSegment")
        };
        assert_eq!(decoded.transfer_id, 42);
        assert_eq!(&decoded.data[..], b"hello bundle");
        assert_eq!(decoded.transfer_extensions.len(), 1);
        let ext = &decoded.transfer_extensions[0];
        assert!(!ext.flags.critical);
        assert_eq!(ext.item_type, 0x0001);
        assert_eq!(ext.item_length, 8);
        assert_eq!(&ext.item_value[..], &ext_value[..]);

        let mut codec = test_codec();
        let mut buf = BytesMut::new();
        codec
            .encode(Message::TransferSegment(segment()), &mut buf)
            .unwrap();
        codec.encode(Message::Keepalive, &mut buf).unwrap();

        let Some(Message::TransferSegment(first)) = codec.decode(&mut buf).unwrap() else {
            panic!("expected TransferSegment")
        };
        assert_eq!(first.transfer_extensions.len(), 1);
        assert_eq!(&first.data[..], b"hello bundle");
        let Some(Message::Keepalive) = codec.decode(&mut buf).unwrap() else {
            panic!("expected Keepalive")
        };
        assert!(buf.is_empty());
    }

    // ---- Inbound segment bound ----

    // The 18-byte XFER_SEGMENT header declaring `data_length`, no data.
    fn xfer_segment_header(data_length: u64) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u8(MessageType::XFER_SEGMENT as u8);
        buf.put_u8(0x01); // END, so no extension list follows the header
        buf.put_u64(1); // transfer id
        buf.put_u64(data_length);
        buf
    }

    // A declared data length beyond the local segment MRU is rejected at
    // decode, before the framed buffer commits to it: waiting for the
    // declared bytes would otherwise buffer the peer's stream without limit,
    // and a 32-bit `as usize` of the length would truncate it and
    // desynchronise framing.
    #[test]
    fn xfer_segment_huge_data_length_is_rejected() {
        for data_length in [u64::MAX, TEST_SEGMENT_MRU + 1] {
            let mut buf = xfer_segment_header(data_length);
            assert!(
                matches!(
                    test_codec().decode(&mut buf),
                    Err(Error::SegmentTooLarge {
                        data_length: got,
                        segment_mru: TEST_SEGMENT_MRU,
                    }) if got == data_length
                ),
                "data length {data_length} must be rejected"
            );
        }

        // The bound is the MRU itself: a maximum-size segment still decodes.
        let mut buf = xfer_segment_header(TEST_SEGMENT_MRU);
        buf.put_bytes(0x42, usize::try_from(TEST_SEGMENT_MRU).unwrap());
        let Some(Message::TransferSegment(decoded)) = test_codec().decode(&mut buf).unwrap() else {
            panic!("expected TransferSegment")
        };
        assert_eq!(decoded.data.len() as u64, TEST_SEGMENT_MRU);
        assert!(buf.is_empty());
    }

    // ---- MSG_REJECT ----

    // UT-TCP-01: MSG_REJECT round-trip for every defined reason code plus
    // unassigned and private values, and rejection of a rejected-message
    // octet that is not a message type we could have sent.
    #[test]
    fn serdes_msg_reject() {
        let cases = [
            (1u8, MessageRejectionReasonCode::UnknownType),
            (2, MessageRejectionReasonCode::Unsupported),
            (3, MessageRejectionReasonCode::Unexpected),
            (0, MessageRejectionReasonCode::Unassigned(0)),
            (42, MessageRejectionReasonCode::Unassigned(42)),
            (0xF5, MessageRejectionReasonCode::Private(0xF5)),
        ];

        for (byte, expected) in cases {
            let decoded: MessageRejectionReasonCode = byte.into();
            assert_eq!(decoded, expected, "wrong variant for byte {byte}");
            assert_eq!(
                u8::from(decoded.clone()),
                byte,
                "round-trip failed for {expected:?}"
            );

            let buf = encode_msg(Message::Reject(MessageRejectMessage {
                reason_code: decoded,
                rejected_message: MessageType::KEEPALIVE as u8,
            }));
            assert_eq!(buf.len(), 3); // header + reason + rejected message

            let Message::Reject(msg) = decode_msg(&buf).unwrap() else {
                panic!("expected Reject")
            };
            assert_eq!(msg.reason_code, expected);
            assert_eq!(msg.rejected_message, MessageType::KEEPALIVE as u8);
        }

        // The rejected-message octet must itself be a defined message type.
        let mut buf = BytesMut::from(&[MessageType::MSG_REJECT as u8, 1, 0xFF][..]);
        assert!(matches!(
            test_codec().decode(&mut buf),
            Err(Error::InvalidMessageType(0xFF))
        ));
    }

    // ---- Reserved flag bits ----

    // Each flag type captures every reserved bit on decode (receive-side
    // information) and strips them on encode: reserved header flag bits
    // SHALL be 0 from the sender (RFC 9174 Sections 4.8, 5.2.2, 5.2.5,
    // and 6.1).
    #[test]
    fn flags_capture_reserved_bits_and_encode_them_as_zero() {
        let flags = SessionInitExtensionFlags::from(0xFF);
        assert!(flags.critical);
        assert_eq!(flags.reserved, 0xFE);
        assert_eq!(u8::from(flags), 0x01);

        let flags = SessionTermMessageFlags::from(0xFF);
        assert!(flags.reply);
        assert_eq!(flags.reserved, 0xFE);
        assert_eq!(u8::from(flags), 0x01);

        let flags = TransferSegmentMessageFlags::from(0xFF);
        assert!(flags.start);
        assert!(flags.end);
        assert_eq!(flags.reserved, 0xFC);
        assert_eq!(u8::from(flags), 0x03);

        let flags = TransferSegmentExtensionFlags::from(0xFF);
        assert!(flags.critical);
        assert_eq!(flags.reserved, 0xFE);
        assert_eq!(u8::from(flags), 0x01);
    }

    // ---- UT-TCP-05: Reason Codes ----

    // UT-TCP-05: SessionTermReasonCode round-trip for all defined values.
    #[test]
    fn reason_code_sess_term_round_trip() {
        let cases = [
            (0u8, SessionTermReasonCode::Unknown),
            (1, SessionTermReasonCode::IdleTimeout),
            (2, SessionTermReasonCode::VersionMismatch),
            (3, SessionTermReasonCode::Busy),
            (4, SessionTermReasonCode::ContactFailure),
            (5, SessionTermReasonCode::ResourceExhaustion),
        ];

        for (byte, expected) in &cases {
            let decoded: SessionTermReasonCode = (*byte).into();
            assert_eq!(decoded, *expected, "wrong variant for byte {byte}");
            let re_encoded: u8 = decoded.into();
            assert_eq!(re_encoded, *byte, "round-trip failed for {expected:?}");
        }
    }

    // UT-TCP-05: TransferRefuseReasonCode round-trip for all defined values.
    #[test]
    fn reason_code_xfer_refuse_round_trip() {
        let cases: Vec<(u8, TransferRefuseReasonCode)> = vec![
            (0, TransferRefuseReasonCode::Unknown),
            (1, TransferRefuseReasonCode::Completed),
            (2, TransferRefuseReasonCode::NoResources),
            (3, TransferRefuseReasonCode::Retransmit),
            (4, TransferRefuseReasonCode::NotAcceptable),
            (5, TransferRefuseReasonCode::ExtensionFailure),
            (6, TransferRefuseReasonCode::SessionTerminating),
        ];

        for (byte, expected) in &cases {
            let decoded: TransferRefuseReasonCode = (*byte).into();
            assert_eq!(decoded, *expected, "wrong variant for byte {byte}");
            let re_encoded: u8 = decoded.into();
            assert_eq!(re_encoded, *byte, "round-trip failed for {expected:?}");
        }
    }

    // UT-TCP-05: Unassigned and private reason code ranges preserved.
    #[test]
    fn reason_code_unassigned_and_private() {
        // Unassigned range (7..=0xEF)
        let decoded: SessionTermReasonCode = 42u8.into();
        assert!(matches!(decoded, SessionTermReasonCode::Unassigned(42)));
        assert_eq!(u8::from(decoded), 42);

        // Private range (0xF0..=0xFF)
        let decoded: SessionTermReasonCode = 0xF5u8.into();
        assert!(matches!(decoded, SessionTermReasonCode::Private(0xF5)));
        assert_eq!(u8::from(decoded), 0xF5);
    }
}
