/// Shorthand for results whose error is [`enum@Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// Errors from message encoding and decoding.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The message type byte is not recognized.
    #[error("Invalid message type {0:#04x}")]
    InvalidMessageType(u8),

    /// The message type byte is in a bundle-reserved range (BPv6 or BPv7
    /// CBOR, Section 12.1).  Mid-PDU this marks raw bundle bytes, whose
    /// extent cannot be determined without parsing the bundle itself;
    /// decoding stops at this point and the remainder is left unprocessed.
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
