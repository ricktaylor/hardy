use alloc::{boxed::Box, string::String};

use thiserror::Error;
#[derive(Error, Debug)]
pub enum Error {
    #[error("Invalid ipn allocator id {0}")]
    IpnInvalidAllocatorId(u64),

    #[error("Invalid ipn node number {0}")]
    IpnInvalidNodeNumber(u64),

    #[error("Invalid ipn service number {0}")]
    IpnInvalidServiceNumber(u64),

    #[error("Unsupported EID scheme {0}")]
    UnsupportedScheme(u64),

    #[error("Not a NodeId")]
    InvalidNodeId,

    #[error("NodeID and Service have different schemes")]
    MismatchedService,

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn core::error::Error + Send + Sync>,
    },

    /// Indicates a violation of the canonical CBOR encoding requirements
    /// from RFC 9171 §4.1 — non-shortest scalar encoding, non-shortest
    /// array head, or unexpected tags in an EID field (refused from the
    /// tag's first byte, without reading the run).
    #[error("EID violates RFC 9171 canonical CBOR encoding requirements")]
    NotCanonical,

    #[error(transparent)]
    InvalidCBOR(hardy_cbor::decode::Error),
}

// Manual rather than `#[from]`: an `UnexpectedTag` from an `Untagged`
// decode is an RFC 9171 §4.1 violation in this domain, so it surfaces as
// `NotCanonical` (see `crate::error` for the rationale).
impl From<hardy_cbor::decode::Error> for Error {
    fn from(e: hardy_cbor::decode::Error) -> Self {
        match e {
            hardy_cbor::decode::Error::UnexpectedTag => Self::NotCanonical,
            e => Self::InvalidCBOR(e),
        }
    }
}

impl crate::error::HasInvalidField for Error {
    fn invalid_field(
        field: &'static str,
        source: Box<dyn core::error::Error + Send + Sync>,
    ) -> Self {
        Error::InvalidField { field, source }
    }
}
