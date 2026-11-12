use alloc::boxed::Box;
use core::fmt;

use hardy_cbor::{
    decode::FromCbor,
    encode::{Encoder, ToCbor},
};
use zeroize::Zeroizing;
/// Block Confidentiality Block (BCB) types and operations (RFC 9172 Section 3.7).
pub mod bcb;
/// Block Integrity Block (BIB) types and operations (RFC 9172 Section 3.6).
pub mod bib;
/// Cryptographic key types and key source abstraction for BPSec operations.
pub mod key;

#[cfg(feature = "bpsec")]
mod reader;
#[cfg(feature = "bpsec")]
pub use reader::DecryptingReader;
/// BPSec-aware editing primitives ([`BPSecEditor`] extension trait on
/// [`crate::editor::Editor`]). Cascade-through-encrypted-BIB block
/// removal, integrity stripping, and decryption.
///
/// [`BPSecEditor`]: edit::BPSecEditor
///
/// Every edit here decrypts or encrypts, so the module is gated with the
/// keyed operations it drives.
#[cfg(feature = "bpsec")]
pub mod edit;

mod error;
pub use error::Error;

mod parse;
pub use parse::UnknownOperation;

/// A key provider function that returns no keys.
/// Use this when parsing bundles that don't require decryption.
pub fn no_keys(_bundle: &crate::bundle::Bundle, _data: &[u8]) -> Box<dyn key::KeySource> {
    Box::new(key::KeySet::EMPTY)
}

/// RFC 9173 default security contexts (BIB-HMAC-SHA2 and BCB-AES-GCM).
#[cfg(feature = "rfc9173")]
pub mod rfc9173;

// Signer and encryptor always compile. Without any security context
// feature enabled (e.g. rfc9173), their `Context` enums only carry the
// `__Reserved` placeholder variant — callers cannot construct a useful
// context, and the build paths return `Error::UnsupportedOperation`.
/// Bundle encryption API for adding BCB blocks to bundles.
#[cfg(feature = "bpsec")]
pub mod encryptor;
/// Bundle signing API for adding BIB blocks to bundles.
#[cfg(feature = "bpsec")]
pub mod signer;

// `crate::Error` is written qualified throughout, deliberately: this
// module's own `Error` (re-exported above) takes the bare name.
use crate::{HashMap, HashSet, error::CaptureFieldErr};

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
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, len) = crate::error::parse_canonical::<u64, _>(data, Error::NotCanonical)?;
        Ok((
            match value {
                #[cfg(feature = "rfc9173")]
                1 => Self::BIB_HMAC_SHA2,
                #[cfg(feature = "rfc9173")]
                2 => Self::BCB_AES_GCM,
                value => Self::Unrecognised(value),
            },
            true,
            len,
        ))
    }
}

/// The decrypt outcomes of one parse's BCB-covered blocks, carried from one
/// pass over the bundle to the next, so that no pass decrypts a block an
/// earlier one already did: each decrypted block's plaintext, and the blocks
/// whose decrypt failed. A [`DecryptingReader`] is seeded with them
/// ([`with_decrypted`](DecryptingReader::with_decrypted)) and hands them back
/// with its own ([`into_decrypted`](DecryptingReader::into_decrypted)).
#[derive(Default)]
pub struct Decrypted {
    /// Each decrypted block's plaintext, zeroized on drop.
    pub plaintext: HashMap<u64, Zeroizing<Box<[u8]>>>,
    /// The blocks whose ciphertext did not authenticate.
    pub failed: HashSet<u64>,
}

// Manual impl: block numbers only, never plaintext.
impl fmt::Debug for Decrypted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decrypted")
            .field("plaintext", &self.plaintext.keys())
            .field("failed", &self.failed)
            .finish()
    }
}
