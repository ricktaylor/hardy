use aes_kw::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, BlockSizeUser, KeyInit, consts::U16};
use alloc::string::String;
use alloc::vec;

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

pub fn unwrap<C>(kek: &[u8], wrapped_key: &[u8]) -> Result<Vec<u8>, String>
where
    C: BlockCipherDecrypt + BlockSizeUser<BlockSize = U16>,
    aes_kw::AesKw<C>: KeyInit,
{
    let kw = aes_kw::AesKw::<C>::new_from_slice(kek).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; wrapped_key.len().saturating_sub(8)];
    kw.unwrap_key(wrapped_key, &mut buf)
        .map(|out| out.to_vec())
        .map_err(|e| e.to_string())
}
