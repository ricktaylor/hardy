/*!
This module defines the primary error type for the `bpv7` crate.

The `Error` enum covers a wide range of issues that can occur during bundle
processing, from parsing errors to semantic validation failures.
*/

use super::*;
use thiserror::Error;

/// The primary error type for the `bpv7` crate.
#[derive(Error, Debug)]
pub enum Error {
    /// Indicates that there is additional data after the end of a CBOR array in a bundle.
    #[error("Bundle has additional data after end of CBOR array")]
    AdditionalData,

    /// Indicates that the bundle protocol version is unsupported.
    #[error("Unsupported bundle protocol version {0}")]
    InvalidVersion(u64),

    /// Indicates that a bundle is missing the required payload block.
    #[error("Bundle has no payload block")]
    MissingPayload,

    /// Indicates that the primary block is not protected by a BPSec BIB or a CRC.
    #[error("Primary block is not protected by a BPSec BIB or a CRC")]
    MissingIntegrityCheck,

    /// Indicates that the bundle payload block has an invalid block number (must be 1).
    #[error("Bundle payload block must be block number 1")]
    InvalidPayloadBlockNumber,

    /// Indicates that the final block of a bundle is not a payload block.
    #[error("Final block of bundle is not a payload block")]
    PayloadNotFinal,

    /// Indicates that a bundle has more than one block with the same block number.
    #[error("Bundle has more than one block with block number {0}")]
    DuplicateBlockNumber(u64),

    /// Indicates that a block has an invalid block number for its type.
    #[error("{1:?} block cannot be block number {0}")]
    InvalidBlockNumber(u64, block::Type),

    /// Indicates that the fragment information is invalid (e.g., offset is greater than total length).
    #[error("Invalid fragment information: offset {0}, total length {1}")]
    InvalidFragmentInfo(u64, u64),

    /// Indicates that a bundle has multiple blocks of a type that should be unique.
    #[error("Bundle has multiple {0:?} blocks")]
    DuplicateBlocks(block::Type),

    /// Indicates that the bundle source has no clock and there is no Bundle Age extension block.
    #[error("Bundle source has no clock, and there is no Bundle Age extension block")]
    MissingBundleAge,

    /// Indicates that a block has an unsupported block type or block content sub-type.
    #[error("Block {0} has an unsupported block type or block content sub-type")]
    Unsupported(u64),

    /// Indicates that a bundle or block has an invalid combination of flags.
    #[error("Invalid bundle or block flag combination")]
    InvalidFlags,

    /// Indicates that a bundle has been altered since it was parsed.
    #[error("Bundle has been altered since parsing")]
    Altered,

    /// Indicates that a bundle does not contain a block
    /// Usually returned from an accessor function, such as decrypt_block
    #[error("Bundle does not contain block {0}")]
    MissingBlock(u64),

    /// An error related to BPSec processing.
    #[error(transparent)]
    InvalidBPSec(#[from] bpsec::Error),

    /// An error related to CRC processing.
    #[error(transparent)]
    InvalidCrc(#[from] crc::Error),

    /// An error related to Endpoint ID processing.
    #[error(transparent)]
    InvalidEid(#[from] eid::Error),

    /// An error related to CBOR decoding.
    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),

    /// A generic error for when parsing a specific field fails.
    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        /// The name of the field that failed to parse.
        field: &'static str,
        /// The underlying error that caused the failure.
        source: Box<dyn core::error::Error + Send + Sync>,
    },
}

/// A trait for mapping errors to a `Error::InvalidField`.
/// This is useful for providing more context when a parsing error occurs.
pub trait CaptureFieldErr<T> {
    /// Maps the error to a `Error::InvalidField` with the given field name.
    fn map_field_err(self, field: &'static str) -> Result<T, Error>;
}

impl<T, E: Into<Box<dyn core::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for core::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, Error> {
        self.map_err(|e| Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}
