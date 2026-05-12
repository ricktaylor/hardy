use alloc::boxed::Box;
use alloc::string::ToString;

use crate::block::{Block, Payload};
use crate::bundle::Bundle;
use hardy_cbor::decode::{Error as DecodeError, FromCbor, parse as cbor_parse};
use hardy_cbor::encode::{Encoder, ToCbor};

/// Block Confidentiality Block (BCB) types and operations (RFC 9172 Section 3.7).
pub mod encrypt;
/// Cryptographic key types and key source abstraction for BPSec operations.
pub mod key;
/// Block Integrity Block (BIB) types and operations (RFC 9172 Section 3.6).
pub mod sign;

pub mod asb;
mod error;
#[cfg(feature = "rfc9173")]
pub(crate) mod key_wrap;

/// High-level security policy API for applying BIB/BCB operations by block type.
#[cfg(feature = "bpsec")]
pub mod policy;

pub use error::Error;

#[cfg(feature = "rfc9173")]
pub(crate) fn rand_bytes<const N: usize>() -> Result<Box<[u8]>, Error> {
    use alloc::vec;
    use rand::TryRng;
    let mut buf = vec![0u8; N].into_boxed_slice();
    rand::rngs::SysRng
        .try_fill_bytes(&mut buf)
        .map_err(|e| Error::Algorithm(e.to_string()))?;
    Ok(buf)
}

/// A key provider function that returns no keys.
/// Use this when parsing bundles that don't require decryption.
pub fn no_keys(_bundle: &Bundle, _data: &[u8]) -> Box<dyn key::KeySource> {
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

impl ToCbor for Context {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(match self {
            #[cfg(feature = "rfc9173")]
            Self::BIB_HMAC_SHA2 => &1,
            #[cfg(feature = "rfc9173")]
            Self::BCB_AES_GCM => &2,
            Self::Unrecognised(v) => v,
        })
    }
}

impl FromCbor for Context {
    type Error = DecodeError;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        cbor_parse::<(u64, bool, usize)>(data).map(|(value, shortest, len)| {
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
    fn block(&'a self, block_number: u64) -> Option<(&'a Block, Option<Payload<'a>>)>;
}
