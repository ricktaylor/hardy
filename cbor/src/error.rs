//! Error type for the streaming CBOR codec.

use thiserror::Error;

/// Errors from the CBOR codec.
#[derive(Error, Debug)]
pub enum Error {
    /// The next CBOR item has a different major type than expected.
    #[error("unexpected CBOR type: expected {expected}, found major type {actual}")]
    UnexpectedType { expected: &'static str, actual: u8 },

    /// The CBOR encoding is structurally invalid.
    #[error("invalid CBOR encoding")]
    InvalidCbor,

    /// A text string contains invalid UTF-8.
    #[error("invalid UTF-8 in text string")]
    InvalidUtf8,

    /// An I/O error from the underlying reader or writer.
    #[error(transparent)]
    Io(#[from] hardy_io::Error),
}
