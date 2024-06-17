use super::*;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::{
    bytes::{Buf, BufMut, BytesMut},
    codec::Decoder,
};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Invalid message type {0}")]
    InvalidMessageType(u8),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("Invalid Node Id string: {0}")]
    InvalidNodeIdUtf8(#[from] std::string::FromUtf8Error),

    #[error("Invalid Node Id: {0}")]
    InvalidNodeId(#[from] bpv7::EidError),
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
    pub node_id: Option<bpv7::Eid>,
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
        dst.put_u32(self.session_extensions.len() as u32);
        for extension in self.session_extensions {
            dst.put_u8(extension.flags.into());
            dst.put_u16(extension.item_type);
            dst.put_u32(extension.item_length);
            dst.put(extension.item_value.as_slice());
        }
        Ok(())
    }

    fn decode(src: &mut BytesMut) -> Result<Option<Message>, Error> {
        if src.len() < 20 {
            // Not enough data to read session init message
            return Ok(None);
        }

        let mut src_cloned = src.clone();
        let keepalive_interval = src_cloned.get_u16();
        let segment_mru = src_cloned.get_u64();
        let transfer_mru = src_cloned.get_u64();
        let node_id_length = src_cloned.get_u16();
        let node_id = if node_id_length > 0 {
            if src_cloned.len() < node_id_length as usize {
                // Not enough data to read node id
                return Ok(None);
            }
            Some(
                String::from_utf8(src_cloned.split_to(node_id_length as usize).into())?
                    .parse::<bpv7::Eid>()?,
            )
        } else {
            None
        };

        if src_cloned.len() < 4 {
            // Not enough data to read session extensions length
            return Ok(None);
        }
        let session_extensions_length = src_cloned.get_u32();
        let mut consumed = 24 + node_id_length as usize;
        let mut session_extensions = Vec::new();
        for _ in 0..session_extensions_length {
            if src_cloned.len() < 5 {
                // Not enough data to read session extension
                return Ok(None);
            }
            let flags = src_cloned.get_u8().into();
            let item_type = src_cloned.get_u16();
            let item_length = src_cloned.get_u32();
            if src_cloned.len() < item_length as usize {
                // Not enough data to read item value
                return Ok(None);
            }
            session_extensions.push(SessionInitExtension {
                flags,
                item_type,
                item_length,
                item_value: src_cloned.split_to(item_length as usize).to_vec(),
            });
            consumed += 5 + item_length as usize;
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
    pub item_length: u32,
    pub item_value: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct SessionInitExtensionFlags {
    pub critical: bool,
}

impl From<u8> for SessionInitExtensionFlags {
    fn from(value: u8) -> Self {
        let mut flags = Self::default();
        if value & 1 != 0 {
            flags.critical = true;
        }

        if value & 0xFE != 0 {
            trace!(
                "Parsing session initialization extension with reserved flag bits set: {value:#x}"
            );
        }
        flags
    }
}

impl From<SessionInitExtensionFlags> for u8 {
    fn from(value: SessionInitExtensionFlags) -> u8 {
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
        if src.len() < 2 {
            // Not enough data to read session term message
            Ok(None)
        } else {
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
}

impl From<u8> for SessionTermMessageFlags {
    fn from(value: u8) -> Self {
        let mut flags = Self::default();
        if value & 1 != 0 {
            flags.reply = true;
        }

        if value & 0xFE != 0 {
            trace!("Parsing session term message with reserved flag bits set: {value:#x}");
        }
        flags
    }
}

impl From<SessionTermMessageFlags> for u8 {
    fn from(value: SessionTermMessageFlags) -> u8 {
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
                trace!("Parsing session term message with unassigned reason code: {n}");
                Self::Unassigned(n)
            }
            n @ 0xF0..=0xFF => {
                trace!("Parsing session term message with private reason code: {n}");
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
    pub rejected_message: MessageType,
}

impl MessageRejectMessage {
    fn encode(self, dst: &mut BytesMut) -> Result<(), Error> {
        dst.put_u8(MessageType::MSG_REJECT as u8);
        dst.put_u8(self.reason_code.into());
        dst.put_u8(self.rejected_message as u8);
        Ok(())
    }

    fn decode(src: &mut BytesMut) -> Result<Option<Message>, Error> {
        if src.len() < 2 {
            // Not enough data to read message reject message
            Ok(None)
        } else {
            Ok(Some(Message::Reject(MessageRejectMessage {
                reason_code: src.get_u8().into(),
                rejected_message: src.get_u8().try_into()?,
            })))
        }
    }
}

#[derive(Debug)]
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
                trace!("Parsing rejection message with unassigned reason code: {n}");
                Self::Unassigned(n)
            }
            n @ 0xF0..=0xFF => {
                trace!("Parsing rejection message with private reason code: {n}");
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
        if src.len() < 9 {
            // Not enough data to read transfer refuse message
            Ok(None)
        } else {
            Ok(Some(Message::TransferRefuse(TransferRefuseMessage {
                reason_code: src.get_u8().into(),
                transfer_id: src.get_u64(),
            })))
        }
    }
}

#[derive(Debug)]
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
                trace!("Parsing transfer refuse message with unassigned reason code: {n}");
                Self::Unassigned(n)
            }
            n @ 0xF0..=0xFF => {
                trace!("Parsing transfer refuse message with private reason code: {n}");
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
    pub message_flags: TransferAckMessageFlags,
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
        if src.len() < 17 {
            // Not enough data to read transfer ack message
            Ok(None)
        } else {
            Ok(Some(Message::TransferAck(TransferAckMessage {
                message_flags: src.get_u8().into(),
                transfer_id: src.get_u64(),
                acknowledged_length: src.get_u64(),
            })))
        }
    }
}

#[derive(Debug, Default)]
pub struct TransferAckMessageFlags {
    pub start: bool,
    pub end: bool,
}

impl From<u8> for TransferAckMessageFlags {
    fn from(value: u8) -> Self {
        let mut flags = Self::default();
        if value & 1 != 0 {
            flags.end = true;
        }
        if value & 2 != 0 {
            flags.start = true;
        }

        if value & 0xFC != 0 {
            trace!("Parsing transfer ack message with reserved flag bits set: {value:#x}");
        }
        flags
    }
}

impl From<TransferAckMessageFlags> for u8 {
    fn from(value: TransferAckMessageFlags) -> u8 {
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

impl From<Message> for MessageType {
    fn from(value: Message) -> Self {
        match value {
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

#[derive(Debug)]
pub struct TransferSegmentMessage {
    pub message_flags: TransferSegmentMessageFlags,
    pub transfer_id: u64,
    pub transfer_extensions: Vec<TransferSegmentExtension>,
    pub data: Vec<u8>,
}

impl TransferSegmentMessage {
    fn encode(self, dst: &mut BytesMut) -> Result<(), Error> {
        dst.put_u8(MessageType::XFER_SEGMENT as u8);
        dst.put_u8(self.message_flags.clone().into());
        dst.put_u64(self.transfer_id);
        if self.message_flags.start {
            dst.put_u32(self.transfer_extensions.len() as u32);
            for extension in self.transfer_extensions {
                dst.put_u8(extension.flags.into());
                dst.put_u16(extension.item_type);
                dst.put_u32(extension.item_length);
                dst.put(extension.item_value.as_slice());
            }
        }
        dst.put_u64(self.data.len() as u64);
        dst.put(self.data.as_slice());
        Ok(())
    }

    fn decode(src: &mut BytesMut) -> Result<Option<Message>, Error> {
        if src.len() < 9 {
            // Not enough data to read transfer segment message
            return Ok(None);
        }
        let mut src_cloned = src.clone();
        let message_flags: TransferSegmentMessageFlags = src_cloned.get_u8().into();
        let transfer_id = src_cloned.get_u64();

        let mut consumed = 9;
        let mut transfer_extensions = Vec::new();
        if message_flags.start {
            if src_cloned.len() < 4 {
                // Not enough data to read transfer extensions length
                return Ok(None);
            }
            let transfer_extensions_length = src_cloned.get_u32();
            consumed += 4;
            for _ in 0..transfer_extensions_length {
                if src_cloned.len() < 5 {
                    // Not enough data to read transfer extension
                    return Ok(None);
                }
                let flags = src_cloned.get_u8().into();
                let item_type = src_cloned.get_u16();
                let item_length = src_cloned.get_u32();
                if src_cloned.len() < item_length as usize {
                    // Not enough data to read item value
                    return Ok(None);
                }
                transfer_extensions.push(TransferSegmentExtension {
                    flags,
                    item_type,
                    item_length,
                    item_value: src_cloned.split_to(item_length as usize).to_vec(),
                });
                consumed += 5 + item_length as usize;
            }
        }
        if src_cloned.len() < 8 {
            // Not enough data to read data length
            return Ok(None);
        }
        let data_length = src_cloned.get_u64();
        if src_cloned.len() < data_length as usize {
            // Not enough data to read data
            return Ok(None);
        }
        let data = src_cloned.split_to(data_length as usize).to_vec();
        src.advance(consumed + 8 + data_length as usize);
        Ok(Some(Message::TransferSegment(TransferSegmentMessage {
            message_flags,
            transfer_id,
            transfer_extensions,
            data,
        })))
    }
}

#[derive(Debug, Default, Clone)]
pub struct TransferSegmentMessageFlags {
    pub start: bool,
    pub end: bool,
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

        if value & 0xFC != 0 {
            trace!("Parsing transfer segment message with reserved flag bits set: {value:#x}");
        }
        flags
    }
}

impl From<TransferSegmentMessageFlags> for u8 {
    fn from(value: TransferSegmentMessageFlags) -> u8 {
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
    pub item_length: u32,
    pub item_value: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct TransferSegmentExtensionFlags {
    pub critical: bool,
}

impl From<u8> for TransferSegmentExtensionFlags {
    fn from(value: u8) -> Self {
        let mut flags = Self::default();
        if value & 1 != 0 {
            flags.critical = true;
        }

        if value & 0xFE != 0 {
            trace!("Parsing transfer segment extension with reserved flag bits set: {value:#x}");
        }
        flags
    }
}

impl From<TransferSegmentExtensionFlags> for u8 {
    fn from(value: TransferSegmentExtensionFlags) -> u8 {
        let mut flags = 0;
        if value.critical {
            flags |= 1;
        }
        flags
    }
}

pub struct MessageCodec {}

impl MessageCodec {
    pub fn new_framed<T: AsyncRead + AsyncWrite + Sized>(
        io: T,
    ) -> tokio_util::codec::Framed<T, Self> {
        Self {}.framed(io)
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

        match src.get_u8().try_into()? {
            MessageType::XFER_SEGMENT => TransferSegmentMessage::decode(src),
            MessageType::XFER_ACK => TransferAckMessage::decode(src),
            MessageType::XFER_REFUSE => TransferRefuseMessage::decode(src),
            MessageType::KEEPALIVE => Ok(Some(Message::Keepalive)),
            MessageType::SESS_TERM => SessionTermMessage::decode(src),
            MessageType::MSG_REJECT => MessageRejectMessage::decode(src),
            MessageType::SESS_INIT => SessionInitMessage::decode(src),
        }
    }
}

impl tokio_util::codec::Encoder<Message> for MessageCodec {
    type Error = Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
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
        }
    }
}
