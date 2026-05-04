use thiserror::Error;

/// Errors that can occur during BTP-U operations.
#[derive(Debug, Error)]
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

    /// A message does not fit in the remaining PDU space.
    #[error(
        "PDU overflow: message of {message_size} bytes exceeds remaining PDU space of {remaining}"
    )]
    PduOverflow {
        message_size: usize,
        remaining: usize,
    },

    /// A Bundle Length hint has an invalid size (must be 1, 2, 4, or 8).
    #[error("Invalid Bundle Length hint size {0} (must be 1, 2, 4, or 8)")]
    InvalidBundleLengthHintSize(u8),

    /// A transfer number is outside the current receive window.
    #[error("Transfer {transfer_number} is outside the current window")]
    TransferOutsideWindow { transfer_number: u32 },

    /// The referenced transfer was previously cancelled.
    #[error("Transfer {transfer_number} was cancelled")]
    TransferCancelled { transfer_number: u32 },

    /// The sender's transfer window is full; no more transfers can be started.
    #[error("Transfer window full (size {window_size})")]
    WindowFull { window_size: u16 },

    /// FEC and core transfer messages were mixed in the same transfer.
    #[error("Cannot mix FEC and core transfer messages in transfer {0}")]
    FecCoreMixing(u32),
}
