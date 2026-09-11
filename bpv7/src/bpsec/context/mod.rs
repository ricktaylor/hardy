use alloc::{boxed::Box, vec};

use hardy_cbor::{
    decode::FromCbor,
    encode::{Encoder, ToCbor},
};
use rand::TryRng;

use crate::canonical::parse_canonical;

pub mod bcb_aes_gcm;
pub mod bib_hmac_sha2;

mod error;

pub use self::error::{Error, Result};

fn rand_bytes<const N: usize>() -> super::Result<Box<[u8]>> {
    let mut buf = vec![0u8; N].into_boxed_slice();
    rand::rngs::SysRng
        .try_fill_bytes(&mut buf)
        .map_err(|_| super::Error::Rng)?;
    Ok(buf)
}

fn rand_array<const N: usize>() -> super::Result<[u8; N]> {
    let mut buf = [0u8; N];
    rand::rngs::SysRng
        .try_fill_bytes(&mut buf)
        .map_err(|_| super::Error::Rng)?;
    Ok(buf)
}

// Tests live in `bpv7/tests/context.rs` (integration tests using the
// public API — keys, Signer/Encryptor/Editor — per the
// inline-tests-vs-tests/ split convention).

/// Scope flags controlling which bundle fields are included in the IPPT (RFC 9173 Section 3.3/4.3).
#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub struct ScopeFlags {
    /// Include the primary block in the Integrity-Protected Plaintext (bit 0).
    pub include_primary_block: bool,
    /// Include the target block header in the IPPT (bit 1).
    pub include_target_header: bool,
    /// Include the security block header in the IPPT (bit 2).
    pub include_security_header: bool,
    /// Any unrecognized scope flag bits, preserved for forward compatibility.
    pub unrecognised: Option<u64>,
}

impl ScopeFlags {
    pub const NONE: Self = Self {
        include_primary_block: false,
        include_target_header: false,
        include_security_header: false,
        unrecognised: None,
    };
}

impl Default for ScopeFlags {
    fn default() -> Self {
        Self {
            include_primary_block: true,
            include_target_header: true,
            include_security_header: true,
            unrecognised: None,
        }
    }
}

impl FromCbor for ScopeFlags {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> core::result::Result<(Self, bool, usize), Self::Error> {
        let (value, len) = parse_canonical::<u64, _>(data, Error::NotCanonical)?;
        let mut flags = Self {
            include_primary_block: false,
            include_target_header: false,
            include_security_header: false,
            unrecognised: None,
        };
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

        if unrecognised != 0 {
            flags.unrecognised = Some(unrecognised);
        }
        Ok((flags, true, len))
    }
}

impl ToCbor for ScopeFlags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        let mut flags = self.unrecognised.unwrap_or(0);
        if self.include_primary_block {
            flags |= 1 << 0;
        }
        if self.include_target_header {
            flags |= 1 << 1;
        }
        if self.include_security_header {
            flags |= 1 << 2;
        }
        encoder.emit(&flags)
    }
}
