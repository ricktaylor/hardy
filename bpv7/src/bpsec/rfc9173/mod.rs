use alloc::{borrow::Cow, boxed::Box, string::ToString, vec};
use core::hash::{Hash, Hasher};

use hardy_cbor::{
    decode::{FromCbor, parse_exact},
    encode::{Encoder, ToCbor},
};
use rand::TryRng;

use crate::{bpsec::Error, primary_block};
pub(crate) mod bcb_aes_gcm;
pub(crate) mod bib_hmac_sha2;

mod iv;
mod key_wrap;
mod mac_tag;

/// Return the bytes to feed into BPSec IPPT/AAD for the primary block.
///
/// - Already canonical (`bool = true`): borrows `raw` — zero copy.
/// - Non-canonical (`bool = false`): re-emits to canonical form (owned).
///   Errors if re-encoding fails — once non-canonical is confirmed we
///   cannot silently fall back to raw bytes and produce a wrong IPPT/AAD.
/// - Parse fails: errors — we cannot verify or produce a canonical form.
pub(super) fn canonical_primary(raw: &[u8]) -> Result<Cow<'_, [u8]>, Error> {
    match parse_exact::<(primary_block::PrimaryBlock, bool)>(raw) {
        Ok((_, true)) => Ok(Cow::Borrowed(raw)),
        Ok((pb, false)) => pb.emit().map(Cow::Owned).map_err(|_| Error::NotCanonical),
        Err(_) => Err(Error::NotCanonical),
    }
}

fn rand_bytes<const N: usize>() -> Result<Box<[u8]>, Error> {
    let mut buf = vec![0u8; N].into_boxed_slice();
    rand::rngs::SysRng
        .try_fill_bytes(&mut buf)
        .map_err(|e| Error::Algorithm(e.to_string()))?;
    Ok(buf)
}

fn rand_array<const N: usize>() -> Result<[u8; N], Error> {
    let mut buf = [0u8; N];
    rand::rngs::SysRng
        .try_fill_bytes(&mut buf)
        .map_err(|e| Error::Algorithm(e.to_string()))?;
    Ok(buf)
}

// Tests live in `bpv7/tests/rfc9173.rs` (integration tests using the
// public API — keys, Signer/Encryptor/Editor — per the
// inline-tests-vs-tests/ split convention).

/// Scope flags controlling which bundle fields are included in the IPPT (RFC 9173 Section 3.3/4.3).
///
/// A bit carried in [`unrecognised`](Self::unrecognised) that names a flag
/// is an alias of that flag: it encodes as the flag's bit, so the verifier
/// includes what the bit names. Equality and hashing compare what the value
/// encodes, so an alias equals its named flag. Signing and encryption
/// [`canonicalize`](Self::canonicalize) the scope before computing the
/// IPPT or AAD, so both sides agree on what it covers.
#[derive(Debug, Clone)]
pub struct ScopeFlags {
    /// Include the primary block in the Integrity-Protected Plaintext (bit 0).
    pub include_primary_block: bool,
    /// Include the target block header in the IPPT (bit 1).
    pub include_target_header: bool,
    /// Include the security block header in the IPPT (bit 2).
    pub include_security_header: bool,
    /// Any unrecognised scope flag bits, preserved for forward
    /// compatibility; zero when there are none.
    pub unrecognised: u64,
}

impl ScopeFlags {
    /// The empty scope: every flag clear, no unrecognised bits. The
    /// [`Default`] scope is RFC 9173's, all three flags set.
    pub const NONE: Self = Self {
        include_primary_block: false,
        include_target_header: false,
        include_security_header: false,
        unrecognised: 0,
    };

    /// Folds every bit of [`unrecognised`](Self::unrecognised) that names a
    /// flag into its named field; genuinely unrecognised bits are kept.
    ///
    /// A hand-built `unrecognised` encodes bit for bit, so `1 << 0`
    /// *is* `include_primary_block` to the verifier. Code that reads the
    /// named fields must canonicalize first, or it misreads the scope the
    /// bytes carry.
    #[must_use]
    pub fn canonicalize(self) -> Self {
        Self::from(u64::from(&self))
    }

    /// Whether no bit of [`unrecognised`](Self::unrecognised) names a flag —
    /// the form [`canonicalize`](Self::canonicalize) returns. Equality
    /// cannot tell an alias from its canonical form; this can.
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        Self::from(self.unrecognised).unrecognised == self.unrecognised
    }
}

impl PartialEq for ScopeFlags {
    fn eq(&self, other: &Self) -> bool {
        u64::from(self) == u64::from(other)
    }
}

impl Eq for ScopeFlags {}

impl Hash for ScopeFlags {
    fn hash<H: Hasher>(&self, state: &mut H) {
        u64::from(self).hash(state);
    }
}

impl Default for ScopeFlags {
    fn default() -> Self {
        Self {
            include_primary_block: true,
            include_target_header: true,
            include_security_header: true,
            unrecognised: 0,
        }
    }
}

impl From<u64> for ScopeFlags {
    fn from(value: u64) -> Self {
        let mut flags = Self::NONE;
        let mut unrecognised = value;

        if (value & (1 << 0)) != 0 {
            flags.include_primary_block = true;
            unrecognised &= !(1 << 0);
        }
        if (value & (1 << 1)) != 0 {
            flags.include_target_header = true;
            unrecognised &= !(1 << 1);
        }
        if (value & (1 << 2)) != 0 {
            flags.include_security_header = true;
            unrecognised &= !(1 << 2);
        }

        flags.unrecognised = unrecognised;
        flags
    }
}

impl From<&ScopeFlags> for u64 {
    fn from(value: &ScopeFlags) -> Self {
        let mut flags = value.unrecognised;
        if value.include_primary_block {
            flags |= 1 << 0;
        }
        if value.include_target_header {
            flags |= 1 << 1;
        }
        if value.include_security_header {
            flags |= 1 << 2;
        }
        flags
    }
}

impl FromCbor for ScopeFlags {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, len) = crate::error::parse_canonical::<u64, _>(data, Error::NotCanonical)?;
        Ok((Self::from(value), true, len))
    }
}

impl ToCbor for ScopeFlags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(&u64::from(self));
    }
}
