//! The codec's error type.

use bytes::TryGetError;

/// Shorthand for results whose error is [`enum@Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// Errors from message encoding and decoding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The bytes at a message boundary begin an encapsulated bundle in its
    /// native format (`first_byte` 0x06 for BPv6 or 0x80..=0x9F for BPv7,
    /// Section 12.1) whose extent could not be determined: no
    /// [`BundleExtent`](crate::codec::BundleExtent) was supplied, or it
    /// returned `None`.  Per Section 7.3 decoding stops here and the
    /// remainder of the PDU, from `offset`, is left unprocessed.
    ///
    /// `offset` counts from the start of the PDU.  The decoder does not
    /// hand the PDU back, so a caller that wants to inspect `pdu[offset..]`
    /// keeps a clone of the `Bytes` it passed in (a reference-count
    /// increment, not a copy).
    #[error(
        "Encapsulated bundle (first byte {first_byte:#04x}) at offset {offset} of undeterminable extent"
    )]
    EncapsulatedBundle {
        /// The bundle's first byte, which no BTP-U message type uses.
        first_byte: u8,
        /// Where the bundle starts.
        offset: usize,
    },

    /// A message content length exceeds the 20-bit maximum.
    #[error("Message content length {length} exceeds 20-bit maximum ({max})")]
    LengthOverflow {
        /// The content length that would have been encoded.
        length: usize,
        /// The largest content length the header can carry.
        max: usize,
    },

    /// Not enough data to decode a message, header, hint, or encapsulated
    /// bundle.
    ///
    /// When a message or encapsulated bundle runs past the end of a PDU
    /// (the fault that ends [`decode_pdu`](crate::codec::decode_pdu)
    /// iteration), both counts are from the start of the PDU.  A hint
    /// chain that runs past its message's content, or one decoded alone,
    /// counts from the start of the chain; a lone header counts from its
    /// first byte.  A fixed-size field (a transfer number, segment index,
    /// or FEC identifier) cut short describes that one read: `needed` is
    /// the field's size and `available` what remained of the content.
    #[error("Insufficient data: need {needed} bytes, have {available}")]
    InsufficientData {
        /// The bytes the failed read required.
        needed: usize,
        /// The bytes there were.
        available: usize,
    },

    /// A [`Message::Unknown`](crate::codec::message::Message::Unknown)
    /// carries a type value that is not unknown: one defined by the base
    /// protocol or reserved for encapsulated bundles.  Encoding it would
    /// produce a message the decoder reads as something else.
    #[error("Message type {0:#04x} is defined or reserved and cannot be relayed as unknown")]
    NotAnUnknownType(u8),
}

/// A fixed-size field read past the end of a message's content: `needed`
/// and `available` describe the read that failed.
impl From<TryGetError> for Error {
    fn from(e: TryGetError) -> Self {
        Error::InsufficientData {
            needed: e.requested,
            available: e.available,
        }
    }
}
