use super::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EidError {
    #[error("dtn URI node-name is empty")]
    DtnNodeNameEmpty,

    #[error("dtn URI missing name-delim '/'")]
    DtnMissingSlash,

    #[error("dtn URIs must start with '//'")]
    DtnMissingPrefix,

    #[error("dtn URI demux part is empty")]
    DtnEmptyDemuxPart,

    #[error("Invalid ipn allocator id {0}")]
    IpnInvalidAllocatorId(u64),

    #[error("Invalid ipn node number {0}")]
    IpnInvalidNodeNumber(u64),

    #[error("Invalid ipn service number {0}")]
    IpnInvalidServiceNumber(u64),

    #[error("Only 2 or 3 components in an ipn URI")]
    IpnInvalidComponents,

    #[error("Missing scheme separator")]
    MissingScheme,

    #[error("Unknown EID scheme {0}")]
    UnknownScheme(String),

    #[error("Unsupported EID scheme {0}")]
    UnsupportedScheme(u64),

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),

    #[error(transparent)]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
}

pub trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, EidError>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, EidError> {
        self.map_err(|e| EidError::InvalidField {
            field,
            source: e.into(),
        })
    }
}
