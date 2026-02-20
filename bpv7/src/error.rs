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

/// Trait for error types that can represent an invalid field error.
///
/// Implement this trait for error types that have an `InvalidField` variant
/// to enable use of the [`CaptureFieldErr`] extension trait.
pub trait HasInvalidField: Sized {
    /// Creates an invalid field error with the given field name and source error.
    fn invalid_field(
        field: &'static str,
        source: Box<dyn core::error::Error + Send + Sync>,
    ) -> Self;
}

impl HasInvalidField for Error {
    fn invalid_field(
        field: &'static str,
        source: Box<dyn core::error::Error + Send + Sync>,
    ) -> Self {
        Error::InvalidField { field, source }
    }
}

/// Extension trait for `Result` that maps errors to an `InvalidField` variant.
///
/// This is useful for providing more context when a parsing error occurs.
/// The error type `E` is specified on the method, allowing turbofish syntax
/// (`.map_field_err::<Error>("field")`) when type inference is insufficient.
pub trait CaptureFieldErr<T> {
    /// Maps the error to an `InvalidField` error with the given field name.
    fn map_field_err<E: HasInvalidField>(self, field: &'static str) -> Result<T, E>;
}

impl<T, Err> CaptureFieldErr<T> for Result<T, Err>
where
    Err: Into<Box<dyn core::error::Error + Send + Sync>>,
{
    fn map_field_err<E: HasInvalidField>(self, field: &'static str) -> Result<T, E> {
        self.map_err(|e| E::invalid_field(field, e.into()))
    }
}
