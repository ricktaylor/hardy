use super::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Bundle has additional data after end of CBOR array")]
    AdditionalData,

    #[error("Unsupported bundle protocol version {0}")]
    InvalidVersion(u64),

    #[error("Bundle has no payload block")]
    MissingPayload,

    #[error("Primary block is not protected by a BPSec BIB or a CRC")]
    MissingIntegrityCheck,

    #[error("Bundle payload block must be block number 1")]
    InvalidPayloadBlockNumber,

    #[error("Final block of bundle is not a payload block")]
    PayloadNotFinal,

    #[error("Bundle has more than one block with block number {0}")]
    DuplicateBlockNumber(u64),

    #[error("{1} block cannot be block number {0}")]
    InvalidBlockNumber(u64, BlockType),

    #[error("Bundle has multiple {0} blocks")]
    DuplicateBlocks(BlockType),

    #[error("Bundle source has no clock, and there is no Bundle Age extension block")]
    MissingBundleAge,

    #[error("Block {0} has an unsupported block type or block content sub-type")]
    Unsupported(u64),

    #[error("Invalid bundle flag combination")]
    InvalidFlags,

    #[error("Block {0} is not in canonical form")]
    NonCanonical(u64),

    #[error("Invalid bundle: {error}")]
    InvalidBundle {
        bundle: Box<Bundle>,
        reason: StatusReportReasonCode,
        error: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    InvalidBPSec(#[from] bpsec::Error),

    #[error(transparent)]
    InvalidCrc(#[from] crc::Error),

    #[error(transparent)]
    InvalidEid(#[from] eid::EidError),

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

pub trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, Error>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, Error> {
        self.map_err(|e| Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}