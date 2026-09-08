use super::*;
use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};

use aes_kw::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, BlockSizeUser, KeyInit, consts::U16};
use zeroize::Zeroizing;

use crate::bpsec::key::KeyAlgorithm;

/// An AES key-wrap (RFC 3394) algorithm, selected from a JWK `alg` by
/// the security contexts. Owns the per-cipher dispatch for both
/// directions, so the contexts never name the cipher types themselves.
#[derive(Clone, Copy)]
pub enum KeyWrap {
    Aes128,
    Aes192,
    Aes256,
}

impl KeyWrap {
    /// The cipher a bare `A*KW` JWK `alg` names, or `None` for any other
    /// algorithm — including the compound `HS*+A*KW` forms, which name a
    /// MAC alongside the wrap and so belong only to a context that
    /// performs one.
    pub fn from_bare_alg(alg: KeyAlgorithm) -> Option<Self> {
        match alg {
            KeyAlgorithm::A128KW => Some(Self::Aes128),
            KeyAlgorithm::A192KW => Some(Self::Aes192),
            KeyAlgorithm::A256KW => Some(Self::Aes256),
            _ => None,
        }
    }

    /// The cipher a JWK `alg` names in any form, bare or compound: the
    /// wrap half of an `HS*+A*KW` is the same AES-KW as the `A*KW` it
    /// ends in.
    pub fn from_alg(alg: KeyAlgorithm) -> Option<Self> {
        match alg {
            KeyAlgorithm::HS256_A128KW
            | KeyAlgorithm::HS384_A128KW
            | KeyAlgorithm::HS512_A128KW => Some(Self::Aes128),

            KeyAlgorithm::HS256_A192KW
            | KeyAlgorithm::HS384_A192KW
            | KeyAlgorithm::HS512_A192KW => Some(Self::Aes192),

            KeyAlgorithm::HS256_A256KW
            | KeyAlgorithm::HS384_A256KW
            | KeyAlgorithm::HS512_A256KW => Some(Self::Aes256),

            alg => Self::from_bare_alg(alg),
        }
    }

    pub fn wrap_key(self, kek: &[u8], cek: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Self::Aes128 => wrap::<aes_kw::aes::Aes128>(kek, cek),
            Self::Aes192 => wrap::<aes_kw::aes::Aes192>(kek, cek),
            Self::Aes256 => wrap::<aes_kw::aes::Aes256>(kek, cek),
        }
    }

    pub fn unwrap_key(
        self,
        kek: &[u8],
        wrapped_key: &[u8],
    ) -> Result<Zeroizing<Box<[u8]>>, String> {
        match self {
            Self::Aes128 => unwrap::<aes_kw::aes::Aes128>(kek, wrapped_key),
            Self::Aes192 => unwrap::<aes_kw::aes::Aes192>(kek, wrapped_key),
            Self::Aes256 => unwrap::<aes_kw::aes::Aes256>(kek, wrapped_key),
        }
    }
}

fn wrap<C>(kek: &[u8], cek: &[u8]) -> Result<Vec<u8>, String>
where
    C: BlockCipherEncrypt + BlockSizeUser<BlockSize = U16>,
    aes_kw::AesKw<C>: KeyInit,
{
    let kw = aes_kw::AesKw::<C>::new_from_slice(kek).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; cek.len() + 8];
    kw.wrap_key(cek, &mut buf)
        .map(|out| out.to_vec())
        .map_err(|e| e.to_string())
}

// Unwraps an AES-KW wrapped CEK. The plaintext buffer is owned by
// `Zeroizing` while still all-zero, so every exit path, including `?`,
// wipes it on drop.
fn unwrap<C>(kek: &[u8], wrapped_key: &[u8]) -> Result<Zeroizing<Box<[u8]>>, String>
where
    C: BlockCipherDecrypt + BlockSizeUser<BlockSize = U16>,
    aes_kw::AesKw<C>: KeyInit,
{
    let kw = aes_kw::AesKw::<C>::new_from_slice(kek).map_err(|e| e.to_string())?;
    let mut buf: Zeroizing<Box<[u8]>> =
        Zeroizing::new(vec![0u8; wrapped_key.len().saturating_sub(8)].into_boxed_slice());
    kw.unwrap_key(wrapped_key, &mut buf)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}
