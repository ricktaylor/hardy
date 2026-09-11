use alloc::boxed::Box;

use thiserror::Error;

use crate::{bpsec::asb, canonical::HasInvalidField};

/// Errors from decoding a context's parameters and results. Operational
/// failures (missing keys, failed verification, cipher errors) live on
/// [`bpsec::Error`](crate::bpsec::Error), which wraps this leaf
/// transparently.
#[derive(Error, Debug)]
pub enum Error {
    /// An unrecognised security context parameter id.
    #[error("Invalid security context parameter id {0}")]
    InvalidContextParameter(u64),

    /// A required security context parameter is absent.
    #[error("Missing security context parameter id {0}")]
    MissingContextParameter(u64),

    /// An unrecognised security context result id.
    #[error("Invalid security context result id {0}")]
    InvalidContextResult(u64),

    /// An AES-GCM IV outside the RFC 9173 §4.3.1 length range.
    #[error("Invalid AES-GCM IV length {0}, must be 8-16 bytes (RFC 9173 Section 4.3.1)")]
    InvalidIvLength(usize),

    /// The parameters violate RFC 9173 canonical encoding requirements.
    #[error("BPSec context parameters violate canonical CBOR encoding requirements")]
    NotCanonical,

    /// A raw range or byte-string failed the ASB grammar.
    #[error(transparent)]
    Asb(#[from] asb::Error),

    /// A field within the parameters failed to parse.
    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<Error>,
    },

    /// An error occurred during CBOR decoding.
    #[error(transparent)]
    InvalidCBOR(hardy_cbor::decode::Error),
}

// Manual rather than `#[from]`: an `UnexpectedTag` is a canonical
// violation in this domain (see `crate::error` for the rationale).
impl From<hardy_cbor::decode::Error> for Error {
    fn from(e: hardy_cbor::decode::Error) -> Self {
        match e {
            hardy_cbor::decode::Error::UnexpectedTag => Self::NotCanonical,
            e => Self::InvalidCBOR(e),
        }
    }
}

impl HasInvalidField for Error {
    fn invalid_field(field: &'static str, source: Self) -> Self {
        Error::InvalidField {
            field,
            source: Box::new(source),
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;
