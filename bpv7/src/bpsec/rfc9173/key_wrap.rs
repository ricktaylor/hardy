use super::*;
use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};

use aes_kw::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, BlockSizeUser, KeyInit, consts::U16};
use zeroize::{Zeroize, Zeroizing};

pub fn wrap<C>(kek: &[u8], cek: &[u8]) -> Result<Vec<u8>, String>
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

/// The output is the plaintext CEK: zeroized on drop, and the scratch
/// buffer is created at the exact output size so the boxing below never
/// reallocates and no unzeroized copy is left on the heap.
pub fn unwrap<C>(kek: &[u8], wrapped_key: &[u8]) -> Result<Zeroizing<Box<[u8]>>, String>
where
    C: BlockCipherDecrypt + BlockSizeUser<BlockSize = U16>,
    aes_kw::AesKw<C>: KeyInit,
{
    let kw = aes_kw::AesKw::<C>::new_from_slice(kek).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; wrapped_key.len().saturating_sub(8)];
    match kw.unwrap_key(wrapped_key, &mut buf) {
        Ok(out) => {
            debug_assert_eq!(out.len(), buf.len());
            Ok(Zeroizing::new(buf.into_boxed_slice()))
        }
        Err(e) => {
            // A failed unwrap may still have written plaintext into the
            // scratch buffer.
            buf.zeroize();
            Err(e.to_string())
        }
    }
}
