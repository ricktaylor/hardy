use alloc::{boxed::Box, string::String};

use thiserror::Error;

use crate::{
    bpsec::{ContextId, key},
    canonical::HasInvalidField,
};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Block is not the target of a BCB")]
    NotEncrypted,

    #[error("Block is not the target of a BIB")]
    NotSigned,

    #[error("Block {0} may be protected by an encrypted BIB that couldn't be decrypted")]
    MaybeHasBib(u64),

    #[error("Cannot remove BCB {0} without also removing all of its targets")]
    StrandsCiphertext(u64),

    #[error("The security target block is not in the bundle")]
    MissingSecurityTarget,

    #[error("BIBs must not target BIBs or BCBs")]
    InvalidBIBTarget,

    #[error("Unrecognised BPSec context")]
    UnrecognisedContext(u64),

    #[error("The target block has a CRC")]
    CrcPresent,

    #[error("BCBs must not target other BCBs or the primary block")]
    InvalidBCBTarget,

    #[error("A BCB targeting a BIB must share at least one target with it")]
    BCBMustShareTarget,

    #[error(
        "BCBs must have the 'Block must be replicated in every fragment' flag set if one of the targets is the payload block"
    )]
    BCBMustReplicate,

    #[error(
        "BCBs must not have the 'Block must be removed from bundle if it can't be processed' flag set."
    )]
    BCBDeleteFlag,

    #[error("BIBs that target blocks that are targets of BCBs must also be encrypted")]
    BIBMustBeEncrypted,

    #[error(
        "The same security service must not be applied to a security target more than once in a bundle"
    )]
    DuplicateOpTarget,

    #[error("Invalid context {0:?}")]
    InvalidContext(ContextId),

    #[error("Decryption failed")]
    DecryptionFailed,

    #[error("No key was provided for the operation")]
    NoKey,

    #[error("Integrity check failed")]
    IntegrityCheckFailed,

    /// This type is deliberately opaque as to avoid potential side-channel
    /// leakage (e.g. padding oracle).
    #[error("Encryption failed")]
    EncryptionFailed,

    #[error("Invalid key material {1:?} for operation {0:?}")]
    InvalidKey(key::Operation, key::Key),

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<Error>,
    },

    #[error(transparent)]
    InvalidEid(#[from] crate::eid::Error),

    #[error("BPSec block violates RFC 9172 canonical CBOR encoding requirements")]
    NotCanonical,

    #[error("Unsupported operation")]
    UnsupportedOperation,

    #[error(transparent)]
    InvalidCBOR(hardy_cbor::decode::Error),

    #[error("Underlying cryptographic operation failed: {0}")]
    Algorithm(String),

    /// Gathering cryptographic randomness failed. Deliberately detail-free:
    /// the failure reason is an environment property, not bundle data, and
    /// the operational errors stay opaque to avoid oracle surfaces.
    #[error("failed to gather cryptographic randomness")]
    Rng,

    /// A structural error from the ASB grammar.
    #[error(transparent)]
    Asb(#[from] super::asb::Error),

    /// A context parameter/result decode error.
    #[cfg(feature = "rfc9173")]
    #[error(transparent)]
    Context(#[from] super::context::Error),

    /// Wrapping a content-encryption key failed.
    #[cfg(feature = "rfc9173")]
    #[error(transparent)]
    KeyWrap(#[from] super::key_wrap::Error),
}

// Manual rather than `#[from]`: an `UnexpectedTag` from an `Untagged`
// decode is an RFC 9172 §4 canonical-encoding violation in this domain,
// so it surfaces as `NotCanonical` (see `crate::error` for the
// rationale).
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
