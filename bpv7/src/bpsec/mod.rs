use alloc::boxed::Box;
use core::{
    fmt,
    ops::{BitOr, BitOrAssign},
};

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
pub mod checks;
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

/// A security service (RFC 9172 §1.1): what one security operation applies
/// to its target, a BCB's confidentiality or a BIB's integrity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecurityService {
    /// A BCB operation: the target is encrypted.
    Confidentiality,
    /// A BIB operation: the target is signed.
    Integrity,
}

/// The security services a caller names on a block, and this node accepts
/// or verifies on it (RFC 9172 §1.1): its BCB operation (confidentiality),
/// its BIB operation (integrity), or both. Two namings of the same block
/// combine with `|`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecurityServices {
    pub confidentiality: bool,
    pub integrity: bool,
}

impl SecurityServices {
    /// The BCB operation alone.
    pub const CONFIDENTIALITY: Self = Self {
        confidentiality: true,
        integrity: false,
    };
    /// The BIB operation alone.
    pub const INTEGRITY: Self = Self {
        confidentiality: false,
        integrity: true,
    };
    /// Both operations.
    pub const BOTH: Self = Self {
        confidentiality: true,
        integrity: true,
    };

    /// Whether either service is named.
    pub fn any(self) -> bool {
        self.confidentiality || self.integrity
    }

    /// The services named here and not in `other`.
    pub fn without(self, other: Self) -> Self {
        Self {
            confidentiality: self.confidentiality && !other.confidentiality,
            integrity: self.integrity && !other.integrity,
        }
    }
}

impl BitOr for SecurityServices {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self {
            confidentiality: self.confidentiality || other.confidentiality,
            integrity: self.integrity || other.integrity,
        }
    }
}

impl BitOrAssign for SecurityServices {
    fn bitor_assign(&mut self, other: Self) {
        *self = *self | other;
    }
}

/// This node's role in a security operation, as a matter of its policy
/// (RFC 9172 §1.4): the verifier checks the operation and leaves it in
/// place; the acceptor processes it and removes it. An acceptor's role
/// outranks a verifier's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Verifier,
    Acceptor,
}

/// This node's roles in the security operations on one block: on its BCB
/// operation (confidentiality) and on its BIB operation (integrity), `None`
/// where it has neither. Two namings of one block combine with `|`, the
/// acceptor's role outranking the verifier's on each operation. The keyed
/// checks take them by block number ([`Parsed::check`](crate::parse::Parsed::check)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Roles {
    pub confidentiality: Option<Role>,
    pub integrity: Option<Role>,
}

impl Roles {
    /// `role` on every service `services` names.
    pub fn each(role: Role, services: SecurityServices) -> Self {
        Self {
            confidentiality: services.confidentiality.then_some(role),
            integrity: services.integrity.then_some(role),
        }
    }

    /// The services this node accepts on the block.
    pub fn accepts(self) -> SecurityServices {
        SecurityServices {
            confidentiality: self.confidentiality == Some(Role::Acceptor),
            integrity: self.integrity == Some(Role::Acceptor),
        }
    }

    /// The services this node verifies on the block.
    pub fn verifies(self) -> SecurityServices {
        SecurityServices {
            confidentiality: self.confidentiality == Some(Role::Verifier),
            integrity: self.integrity == Some(Role::Verifier),
        }
    }
}

impl BitOr for Roles {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self {
            confidentiality: self.confidentiality.max(other.confidentiality),
            integrity: self.integrity.max(other.integrity),
        }
    }
}

impl BitOrAssign for Roles {
    fn bitor_assign(&mut self, other: Self) {
        *self = *self | other;
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
