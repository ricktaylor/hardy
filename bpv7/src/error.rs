/*!
This module defines the primary error type for the `bpv7` crate.

The `Error` enum covers a wide range of issues that can occur during bundle
processing, from parsing errors to semantic validation failures.
*/

use hardy_cbor::decode::{Error as CborError, Untagged};
use thiserror::Error;

use super::*;

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

    /// Indicates that the blocks preceding the payload block's data run past
    /// the 256 MiB implementation bound. Everything before the payload body —
    /// every extension block and the payload block's header — must be
    /// resident for verification, and real header chains are kilobytes; only
    /// the payload body itself may exceed the bound. Detected from the
    /// declared block lengths alone, before any body byte arrives.
    #[error(
        "Blocks before the payload data end at byte {0}, beyond the 256 MiB implementation bound"
    )]
    ExtensionBlocksTooLarge(u64),

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

    /// An error related to CBOR decoding.
    #[error(transparent)]
    InvalidCBOR(hardy_cbor::decode::Error),

    /// A generic error for when parsing a specific field fails.
    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        /// The name of the field that failed to parse.
        field: &'static str,
        /// The underlying error that caused the failure.
        source: Box<dyn core::error::Error + Send + Sync>,
    },
}

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

/// Decode the next element of a CBOR series as `T`, rejecting any
/// non-shortest encoding with the caller's `not_canonical` error. Generic
/// over the error domain — each domain passes its own `NotCanonical`
/// variant, mirroring [`parse_canonical`] — and over the series arity, so
/// bpv7 array fields, BPSec ASB sequences, and status-report fields all
/// share the one implementation. Decodes through [`Untagged`], so a tag
/// run in front of the element is rejected from its first byte without
/// being read; the rejection surfaces as `not_canonical`, never as the
/// raw cbor `UnexpectedTag`.
pub(crate) fn require_canonical<T, E, const D: usize>(
    seq: &mut hardy_cbor::decode::Series<D>,
    field: &'static str,
    not_canonical: E,
) -> Result<T, E>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<CborError> + Into<Box<dyn core::error::Error + Send + Sync>>,
    E: HasInvalidField + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    match seq.parse::<(Untagged<T>, bool)>() {
        Err(e) => {
            // Scalar `T`s surface the `Untagged` rejection as a raw cbor
            // `UnexpectedTag`; translate it to the domain's canonical
            // error, as the domain `From<CborError>` impls already do
            // for composite `T`s.
            let e: Box<dyn core::error::Error + Send + Sync> = e.into();
            if matches!(
                e.downcast_ref::<CborError>(),
                Some(CborError::UnexpectedTag)
            ) {
                Err(E::invalid_field(field, not_canonical.into()))
            } else {
                Err(E::invalid_field(field, e))
            }
        }
        Ok((_, false)) => Err(E::invalid_field(field, not_canonical.into())),
        Ok((Untagged(t), true)) => Ok(t),
    }
}

/// Decode a `T` from the start of `data` in its canonical (shortest) form,
/// returning the value and the bytes consumed. A non-canonical encoding —
/// `T::from_cbor` reporting `shortest == false` — is rejected with
/// `not_canonical`. The whole-slice counterpart of [`require_canonical`] (which
/// decodes an array element), shared by leaf `FromCbor` impls whose wire form is
/// a single bare value: block/CRC type codes, lifetimes, DTN times, BPSec
/// context and variant ids. Generic over the caller's error so each keeps its
/// own `NotCanonical`. Decodes through [`Untagged`], so a tag run in front
/// of the value is rejected from its first byte without being read; the
/// rejection flows through `E`'s `From<T::Error>` conversion, which every
/// bpv7 error domain translates to its own `NotCanonical`.
pub(crate) fn parse_canonical<T, E>(data: &[u8], not_canonical: E) -> Result<(T, usize), E>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<CborError>,
    E: From<T::Error>,
{
    let (Untagged(value), shortest, len) =
        hardy_cbor::decode::parse::<(Untagged<T>, bool, usize)>(data)?;
    if shortest {
        Ok((value, len))
    } else {
        Err(not_canonical)
    }
}
