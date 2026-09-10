/*!
This module defines the primary error type for the `bpv7` crate.

The `Error` enum covers a wide range of issues that can occur during bundle
processing, from parsing errors to semantic validation failures.
*/

use alloc::boxed::Box;

use hardy_cbor::decode::Error as CborError;
use thiserror::Error;

use crate::{block, bpsec, canonical::HasInvalidField, crc, eid, status_report};

/// The primary error type for the `bpv7` crate.
#[derive(Error, Debug)]
pub enum Error {
    /// Indicates that there is additional data after the end of a CBOR array in a bundle.
    #[error("Bundle has additional data after end of CBOR array")]
    AdditionalData,

    /// Indicates that the bundle protocol version is unsupported.
    #[error("Unsupported bundle protocol version {0}")]
    InvalidVersion(u64),

    /// Indicates that the data begins with CBOR unsigned integer 6 — the
    /// first byte of an RFC 5050 (BPv6) primary block — rather than the
    /// outer array of a BPv7 bundle.
    #[error("Possible BPv6 bundle")]
    PossibleBpv6,

    /// Indicates that the data begins with a byte that cannot start a BPv7
    /// bundle: the outer item is not the RFC 9171 §4.1 indefinite-length
    /// CBOR array. Carries the offending first byte.
    #[error("Not a BPv7 bundle (first byte {0:#04x})")]
    NotABundle(u8),

    /// Indicates that a bundle is missing the required payload block.
    #[error("Bundle has no payload block")]
    MissingPayload,

    /// Indicates that a bundle has more than one block with the same block number.
    #[error("Bundle has more than one block with block number {0}")]
    DuplicateBlockNumber(u64),

    /// Indicates that a block has an invalid block number for its type.
    #[error("{1:?} block cannot be block number {0}")]
    InvalidBlockNumber(u64, block::Type),

    /// Indicates that the fragment information is invalid (e.g., offset is greater than total length).
    #[error("Invalid fragment information: offset {0}, total length {1}")]
    InvalidFragmentInfo(u64, u64),

    /// Indicates that a Hop Count Block has a hop limit outside the
    /// RFC 9171 §4.4.3 range (1 through 255).
    #[error("Hop Count Block has invalid hop limit {0} (must be in range 1..=255)")]
    InvalidHopLimit(u64),

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

    /// Indicates that the bundle bytes violate the canonical CBOR encoding
    /// rules required by RFC 9171 (§4.1, §4.2.2, §4.3.2): non-deterministic
    /// field encoding, definite-length outer bundle array, indefinite-length
    /// block-type-specific data byte string, malformed CRC byte string head,
    /// or unexpected CBOR tags (refused from the tag's first byte, without
    /// reading the run).
    #[error("Bundle violates RFC 9171 canonical CBOR encoding requirements")]
    NotCanonical,

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

    /// An error related to status report processing.
    #[error(transparent)]
    InvalidStatusReport(#[from] status_report::Error),

    /// An error related to CBOR decoding.
    #[error(transparent)]
    InvalidCBOR(hardy_cbor::decode::Error),

    /// A generic error for when parsing a specific field fails.
    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        /// The name of the field that failed to parse.
        field: &'static str,
        /// The underlying error that caused the failure.
        source: Box<Error>,
    },
}

pub type Result<T> = core::result::Result<T, Error>;

// Manual rather than `#[from]`: `UnexpectedTag` is the cbor-level signal
// from an `Untagged` decode, and within bpv7 a tag where none is permitted
// is an RFC 9171 §4.1 canonical-encoding violation — so it surfaces as
// `NotCanonical` like every other framing violation, never as a raw cbor
// error. The other error-domain enums (`eid`, `bpsec`, `status_report`)
// make the same translation.
impl From<CborError> for Error {
    fn from(e: CborError) -> Self {
        match e {
            CborError::UnexpectedTag => Self::NotCanonical,
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
