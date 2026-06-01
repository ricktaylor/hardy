use super::*;
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
    /// array head, or unexpected tags in an EID field.
    #[error("EID violates RFC 9171 canonical CBOR encoding requirements")]
    NotCanonical,

    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),
}

impl crate::error::HasInvalidField for Error {
    fn invalid_field(
        field: &'static str,
        source: Box<dyn core::error::Error + Send + Sync>,
    ) -> Self {
        Error::InvalidField { field, source }
    }
}
