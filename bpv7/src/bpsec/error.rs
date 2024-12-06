use super::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Mismatch Target and Results arrays")]
    MismatchedTargetResult,

    #[error("The security target block is not in the bundle")]
    MissingSecurityTarget,

    #[error("Invalid Null or LocalNode security source")]
    InvalidSecuritySource,

    #[error("BIBs must not target BIBs or BCBs")]
    InvalidBIBTarget,

    #[error("Unrecognised BPSec context")]
    UnrecognisedContext(u64),

    #[error(
        "BCBs must not target other BCBs, the primary block, or BIBs that don't share targets"
    )]
    InvalidBCBTarget,

    #[error("Processing failed on an extension block that has 'Delete block on failure' flag set, but is the target of a BCB")]
    InvalidTargetFlags,

    #[error("Invalid security context parameter id {0}")]
    InvalidContextParameter(u64),

    #[error("Missing security context parameter id {0}")]
    MissingContextParameter(u64),

    #[error("Invalid security context result id {0}")]
    InvalidContextResult(u64),

    #[error("Missing security context result id {0}")]
    MissingContextResult(u64),

    #[error("BCBs must have the 'Block must be replicated in every fragment' flag set if one of the targets is the payload block")]
    BCBMustReplicate,

    #[error("BCBs must not have the 'Block must be removed from bundle if it can't be processed' flag set.")]
    BCBDeleteFlag,

    #[error("BCBs must not target a BIB unless it shares a security target with that BIB")]
    BCBMustShareTarget,

    #[error("The same security service must not be applied to a security target more than once in a bundle")]
    DuplicateOpTarget,

    #[error("No targets in BPSec extension block")]
    NoTargets,

    #[error("Invalid context {0}")]
    InvalidContext(Context),

    #[error("Decryption failed")]
    DecryptionFailed,

    #[error("Encryption failed")]
    EncryptionFailed,

    #[error("Integrity check failed")]
    IntegrityCheckFailed,

    #[error("No key material for security operation source {0}")]
    NoKey(Eid),

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),
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
