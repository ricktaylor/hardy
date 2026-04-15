use super::*;

/// Block Confidentiality Block (BCB) types and operations (RFC 9172 Section 3.7).
pub mod bcb;
/// Block Integrity Block (BIB) types and operations (RFC 9172 Section 3.6).
pub mod bib;
/// Cryptographic key types and key source abstraction for BPSec operations.
pub mod key;

mod error;
mod parse;

/// RFC 9173 default security contexts (BIB-HMAC-SHA2 and BCB-AES-GCM).
#[cfg(feature = "rfc9173")]
pub mod rfc9173;

// Signer and encryptor require at least one security context to be enabled.
// The internal "bpsec" feature is automatically enabled by context features
// (rfc9173, and future cose).
/// Bundle encryption API for adding BCB blocks to bundles.
#[cfg(feature = "bpsec")]
pub mod encryptor;
/// Bundle signing API for adding BIB blocks to bundles.
#[cfg(feature = "bpsec")]
pub mod signer;

use crate::error::CaptureFieldErr;

pub use error::Error;

/// A key provider function that returns no keys.
/// Use this when parsing bundles that don't require decryption.
pub fn no_keys(_bundle: &bundle::Bundle, _data: &[u8]) -> Box<dyn key::KeySource> {
    Box::new(key::KeySet::EMPTY)
}

/// BPSec security context identifier (RFC 9172 Section 3.4).
#[derive(Debug, Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
pub enum Context {
    /// BIB-HMAC-SHA2 integrity context (RFC 9173 Section 3).
    #[cfg(feature = "rfc9173")]
    BIB_HMAC_SHA2,
    /// BCB-AES-GCM confidentiality context (RFC 9173 Section 4).
    #[cfg(feature = "rfc9173")]
    BCB_AES_GCM,
    /// A security context ID not recognized by this implementation.
    Unrecognised(u64),
}

impl hardy_cbor::encode::ToCbor for Context {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(match self {
            #[cfg(feature = "rfc9173")]
            Self::BIB_HMAC_SHA2 => &1,
            #[cfg(feature = "rfc9173")]
            Self::BCB_AES_GCM => &2,
            Self::Unrecognised(v) => v,
        })
    }
}

impl hardy_cbor::decode::FromCbor for Context {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data).map(|(value, shortest, len)| {
            (
                match value {
                    #[cfg(feature = "rfc9173")]
                    1 => Self::BIB_HMAC_SHA2,
                    #[cfg(feature = "rfc9173")]
                    2 => Self::BCB_AES_GCM,
                    value => Self::Unrecognised(value),
                },
                shortest,
                len,
            )
        })
    }
}

/// Provides access to bundle blocks by number, used during BPSec IPPT construction.
pub trait BlockSet<'a> {
    /// Returns the block and its payload for the given block number, or `None` if absent.
    fn block(&'a self, block_number: u64)
    -> Option<(&'a block::Block, Option<block::Payload<'a>>)>;
}
