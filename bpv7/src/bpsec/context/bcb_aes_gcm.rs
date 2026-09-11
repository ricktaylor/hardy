use alloc::{boxed::Box, string::ToString, sync::Arc, vec::Vec};
use core::ops::Range;

use aes_gcm::{
    KeyInit,
    aes::cipher::consts::{U8, U9, U10, U11, U12, U13, U14, U15, U16},
};
use hardy_cbor::{
    decode::FromCbor,
    encode::{Array, Encoder, Raw, ToCbor},
};

use super::{ScopeFlags, rand_array, rand_bytes};
use crate::{
    HashMap,
    bpsec::{ContextId, Error, asb, bcb, iv::Iv, key, key_wrap::KeyWrap},
    bundle,
    canonical::parse_canonical,
    eid,
};

/// The RFC 9173 §4.3.2 variant parameter. A foreign wire value is a
/// legitimate RFC 9172 pass-through state, carried as `Unrecognised`;
/// the encrypt key checks never produce it, so it cannot reach the
/// cipher dispatch.
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AesVariant {
    A128GCM,
    #[default]
    A256GCM,
    Unrecognised(u64),
}

impl ToCbor for AesVariant {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        match self {
            Self::A128GCM => encoder.emit(&1),
            Self::A256GCM => encoder.emit(&3),
            Self::Unrecognised(v) => encoder.emit(v),
        }
    }
}

impl FromCbor for AesVariant {
    type Error = super::Error;

    fn from_cbor(data: &[u8]) -> core::result::Result<(Self, bool, usize), Self::Error> {
        let (value, len) = parse_canonical::<u64, _>(data, super::Error::NotCanonical)?;
        Ok((
            match value {
                1 => Self::A128GCM,
                3 => Self::A256GCM,
                v => Self::Unrecognised(v),
            },
            true,
            len,
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Parameters {
    pub iv: Iv,
    pub variant: AesVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: ScopeFlags,
}

impl Parameters {
    fn from_cbor(parameters: HashMap<u64, Range<usize>>, data: &[u8]) -> super::Result<Self> {
        let mut iv = None;
        let mut variant = None;
        let mut key = None;
        let mut flags = None;
        for (id, range) in parameters {
            match id {
                1 => iv = Some(asb::decode_box(range, data)?),
                2 => variant = Some(hardy_cbor::decode::parse(asb::bounded_slice(data, range)?)?),
                3 => key = Some(asb::decode_box(range, data)?),
                4 => flags = Some(hardy_cbor::decode::parse(asb::bounded_slice(data, range)?)?),
                _ => return Err(super::Error::InvalidContextParameter(id)),
            }
        }

        // The RFC 9173 §4.3.1 length bound lives in `Iv::from_bytes`.
        let iv: Box<[u8]> = iv.ok_or(super::Error::MissingContextParameter(1))?;

        Ok(Self {
            iv: Iv::from_bytes(&iv)?,
            variant: variant.unwrap_or_default(),
            key,
            flags: flags.unwrap_or_default(),
        })
    }
}

impl ToCbor for Parameters {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        let mut mask: u32 = 1 << 1;
        if self.variant != AesVariant::default() {
            mask |= 1 << 2;
        }
        if self.key.is_some() {
            mask |= 1 << 3;
        }
        if self.flags != ScopeFlags::default() {
            mask |= 1 << 4;
        }
        encoder.emit_array(Some(mask.count_ones() as usize), |a| {
            for b in 1..=4 {
                if mask & (1 << b) != 0 {
                    match b {
                        1 => a.emit(&(b, &hardy_cbor::encode::Bytes(self.iv.as_slice()))),
                        2 => a.emit(&(b, &self.variant)),
                        3 => a.emit(&(b, &hardy_cbor::encode::Bytes(self.key.as_ref().unwrap()))),
                        4 => a.emit(&(b, &self.flags)),
                        _ => unreachable!("loop range is 1..=4"),
                    }
                }
            }
        })
    }
}

#[derive(Debug)]
pub struct Results(pub Option<Box<[u8]>>);

impl Results {
    fn from_cbor(results: HashMap<u64, Range<usize>>, data: &[u8]) -> super::Result<Self> {
        let mut r = None;
        for (id, range) in results {
            match id {
                1 => r = Some(asb::decode_box(range, data)?),
                _ => return Err(super::Error::InvalidContextResult(id)),
            }
        }

        Ok(Self(r))
    }
}

impl ToCbor for Results {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        if let Some(r) = self.0.as_ref() {
            encoder.emit(&[&(1, &hardy_cbor::encode::Bytes(r))]);
        } else {
            encoder.emit::<[u8; 0]>(&[])
        }
    }
}

fn build_data(flags: &ScopeFlags, args: &bcb::OperationArgs) -> Result<Vec<u8>, Error> {
    let mut encoder = Encoder::new();
    encoder.emit(&ScopeFlags {
        include_primary_block: flags.include_primary_block,
        include_target_header: flags.include_target_header,
        include_security_header: flags.include_security_header,
        ..Default::default()
    });

    if flags.include_primary_block {
        let raw = args
            .blocks
            .block(0)
            .and_then(|v| v.1)
            .expect("Missing primary block!");
        let raw = raw.as_ref();
        // RFC 9172 §4: AAD requires the canonical (deterministic) form.
        encoder.emit(&Raw(
            &bundle::PrimaryBlock::canonical_bytes(raw).map_err(|_| Error::NotCanonical)?
        ));
    }

    if flags.include_target_header {
        let target_block = args
            .blocks
            .block(args.target)
            .ok_or(Error::MissingSecurityTarget)?
            .0;
        encoder.emit(&target_block.block_type);
        encoder.emit(&args.target);
        encoder.emit(&target_block.flags);
    }

    if flags.include_security_header {
        let source_block = args
            .blocks
            .block(args.source)
            .ok_or(Error::MissingSecurityTarget)?
            .0;
        encoder.emit(&source_block.block_type);
        encoder.emit(&args.source);
        encoder.emit(&source_block.flags);
    }

    Ok(encoder.build())
}

// Deliberately generic over the cipher (and hence its nonce size) for the
// 8-16 byte IV round-trip test, so the slice-to-nonce conversion stays.
fn encrypt_inner<C: aes_gcm::aead::Aead>(
    cipher: C,
    iv: &[u8],
    aad: &[u8],
    msg: &[u8],
) -> Result<Box<[u8]>, Error> {
    let nonce = <&aes_gcm::aead::Nonce<C>>::try_from(iv).map_err(|_| Error::EncryptionFailed)?;
    cipher
        .encrypt(nonce, aes_gcm::aead::Payload { msg, aad })
        .map(Into::into)
        .map_err(|_| Error::EncryptionFailed)
}

#[derive(Debug)]
pub struct Operation {
    pub parameters: Arc<Parameters>,
    pub results: Results,
}

impl Operation {
    pub fn is_unsupported(&self) -> bool {
        matches!(self.parameters.variant, AesVariant::Unrecognised(_))
    }

    pub fn encrypt(
        jwk: &key::Key,
        scope_flags: ScopeFlags,
        args: bcb::OperationArgs,
    ) -> Result<(Self, Box<[u8]>), Error> {
        let payload = args
            .blocks
            .block(args.target)
            .ok_or(Error::MissingSecurityTarget)?
            .1
            .ok_or(Error::MissingSecurityTarget)?;

        if let Some(ops) = &jwk.operations
            && !ops.contains(&key::Operation::Encrypt)
        {
            return Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone()));
        }

        // Bind the wrap algorithm once; the wrap step below dispatches
        // through it instead of re-matching `jwk.key_algorithm`. `None`
        // means direct use of the KEK.
        let key_wrap = match &jwk.key_algorithm {
            Some(key::KeyAlgorithm::Direct) | None => None,
            Some(alg) => Some(
                KeyWrap::from_bare_alg(*alg)
                    .ok_or_else(|| Error::InvalidKey(key::Operation::Encrypt, jwk.clone()))?,
            ),
        };

        if key_wrap.is_some()
            && let Some(ops) = &jwk.operations
            && !ops.contains(&key::Operation::WrapKey)
        {
            return Err(Error::InvalidKey(key::Operation::WrapKey, jwk.clone()));
        }

        let variant = match &jwk.enc_algorithm {
            Some(key::EncAlgorithm::A128GCM) => AesVariant::A128GCM,
            None | Some(key::EncAlgorithm::A256GCM) => AesVariant::A256GCM,
            _ => return Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone())),
        };

        let cek = if key_wrap.is_some() {
            Some(zeroize::Zeroizing::from(match variant {
                AesVariant::A128GCM => rand_bytes::<16>()?,
                AesVariant::A256GCM => rand_bytes::<32>()?,
                // Dead in practice: the match above never yields it.
                AesVariant::Unrecognised(_) => {
                    return Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone()));
                }
            }))
        } else {
            None
        };

        let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
            return Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone()));
        };

        let aad = build_data(&scope_flags, &args)?;

        let active_cek = cek.as_ref().map_or(
            kek.expose_secret(),
            |cek: &zeroize::Zeroizing<Box<[u8]>>| cek.as_ref(),
        );

        // 12-byte IV: the RFC 9173 §4.3.1 SHOULD.
        let iv = Iv::B12(rand_array::<12>()?);

        let ciphertext = match variant {
            AesVariant::A128GCM => aes_gcm::Aes128Gcm::new_from_slice(active_cek)
                .map_err(|e| Error::Algorithm(e.to_string()))
                .and_then(|cipher| encrypt_inner(cipher, iv.as_slice(), &aad, payload.as_ref())),
            AesVariant::A256GCM => aes_gcm::Aes256Gcm::new_from_slice(active_cek)
                .map_err(|e| Error::Algorithm(e.to_string()))
                .and_then(|cipher| encrypt_inner(cipher, iv.as_slice(), &aad, payload.as_ref())),
            // Dead in practice: the enc-algorithm match above never yields it.
            AesVariant::Unrecognised(_) => {
                Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone()))
            }
        }?;

        let key = if let (Some(cek), Some(key_wrap)) = (&cek, key_wrap) {
            Some(
                key_wrap
                    .wrap_key(kek.expose_secret(), cek)
                    .map_err(Error::KeyWrap)?
                    .into(),
            )
        } else {
            None
        };

        Ok((
            Self {
                parameters: Arc::new(Parameters {
                    iv,
                    variant,
                    key,
                    flags: scope_flags,
                }),
                results: Results(None),
            },
            ciphertext,
        ))
    }

    pub fn decrypt<K>(
        &self,
        key_source: &K,
        args: bcb::OperationArgs,
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error>
    where
        K: key::KeySource + ?Sized,
    {
        let data = args
            .blocks
            .block(args.target)
            .ok_or(Error::MissingSecurityTarget)?
            .1
            .ok_or(Error::MissingSecurityTarget)?;

        let aad = build_data(&self.parameters.flags, &args)?;

        if let Some(wrapped_cek) = &self.parameters.key {
            // Key wrapping mode - need a KEK to unwrap
            let jwk = key_source
                .key(
                    args.bpsec_source,
                    &[key::Operation::UnwrapKey, key::Operation::Decrypt],
                )
                .ok_or(Error::NoKey)?;

            let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
                return Err(Error::DecryptionFailed);
            };

            let cek = jwk
                .key_algorithm
                .and_then(KeyWrap::from_bare_alg)
                .ok_or(Error::DecryptionFailed)?
                .unwrap_key(kek.expose_secret(), wrapped_cek)
                .map_err(|_| Error::DecryptionFailed)?;

            self.decrypt_middle(jwk.enc_algorithm, cek.as_ref(), &aad, data.as_ref())
        } else {
            // Direct mode - need a decryption key
            let jwk = key_source
                .key(args.bpsec_source, &[key::Operation::Decrypt])
                .ok_or(Error::NoKey)?;

            if let Some(key_algorithm) = jwk.key_algorithm
                && !matches!(key_algorithm, key::KeyAlgorithm::Direct)
            {
                return Err(Error::DecryptionFailed);
            }

            let key::Type::OctetSequence { key: cek } = &jwk.key_type else {
                return Err(Error::DecryptionFailed);
            };

            self.decrypt_middle(jwk.enc_algorithm, cek.expose_secret(), &aad, data.as_ref())
        }
    }

    fn decrypt_middle(
        &self,
        enc_algorithm: Option<key::EncAlgorithm>,
        cek: &[u8],
        aad: &[u8],
        data: &[u8],
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error> {
        match (self.parameters.variant, enc_algorithm) {
            (AesVariant::A128GCM, Some(key::EncAlgorithm::A128GCM)) => {
                self.decrypt_gcm::<aes_gcm::aes::Aes128>(cek, aad, data)
            }
            (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                self.decrypt_gcm::<aes_gcm::aes::Aes256>(cek, aad, data)
            }
            // A foreign wire variant is a legitimate RFC 9172 pass-through
            // state; this node just cannot decrypt it.
            (AesVariant::Unrecognised(_), _) => Err(Error::UnsupportedOperation),
            _ => Err(Error::DecryptionFailed),
        }
    }

    // AES-GCM decryption dispatched on the IV size. RFC 9173 §4.3.1 permits
    // any IV of 8-16 bytes; aes-gcm parameterises the cipher by its nonce
    // size, so each `Iv` arm hands its exact-size array to the corresponding
    // `AesGcm<Aes, Un>` type. Encrypt always emits 12-byte IVs (the RFC's
    // SHOULD); this only widens acceptance on decrypt.
    fn decrypt_gcm<Aes>(
        &self,
        cek: &[u8],
        aad: &[u8],
        data: &[u8],
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error>
    where
        Aes: aes_gcm::aes::cipher::BlockSizeUser<BlockSize = aes_gcm::aes::cipher::consts::U16>
            + aes_gcm::aes::cipher::BlockCipherEncrypt
            + KeyInit,
    {
        macro_rules! decrypt_sized {
            ($n:ty, $iv:expr) => {{
                let cipher = aes_gcm::AesGcm::<Aes, $n>::new_from_slice(cek)
                    .map_err(|_| Error::DecryptionFailed)?;
                // `From<[u8; N]>` is infallible: the arm proves the size.
                self.decrypt_inner(
                    cipher,
                    aes_gcm::aead::Nonce::<aes_gcm::AesGcm<Aes, $n>>::from(*$iv),
                    aad,
                    data,
                )
                .ok_or(Error::DecryptionFailed)
            }};
        }

        // Exhaustive: an out-of-range IV is not a value of the type.
        match &self.parameters.iv {
            Iv::B8(iv) => decrypt_sized!(U8, iv),
            Iv::B9(iv) => decrypt_sized!(U9, iv),
            Iv::B10(iv) => decrypt_sized!(U10, iv),
            Iv::B11(iv) => decrypt_sized!(U11, iv),
            Iv::B12(iv) => decrypt_sized!(U12, iv),
            Iv::B13(iv) => decrypt_sized!(U13, iv),
            Iv::B14(iv) => decrypt_sized!(U14, iv),
            Iv::B15(iv) => decrypt_sized!(U15, iv),
            Iv::B16(iv) => decrypt_sized!(U16, iv),
        }
    }

    fn decrypt_inner<C: aes_gcm::aead::Aead + aes_gcm::aead::AeadInOut>(
        &self,
        cipher: C,
        nonce: aes_gcm::aead::Nonce<C>,
        aad: &[u8],
        msg: &[u8],
    ) -> Option<zeroize::Zeroizing<Box<[u8]>>> {
        let nonce = &nonce;
        if let Some(tag) = self.results.0.as_ref() {
            let tag = <&aes_gcm::aead::Tag<C>>::try_from(tag.as_ref()).ok()?;
            let mut msg = zeroize::Zeroizing::new(Box::<[u8]>::from(msg));
            cipher
                .decrypt_inout_detached(nonce, aad, (&mut msg[..]).into(), tag)
                .ok()
                .map(|_| msg)
        } else {
            cipher
                .decrypt(nonce, aes_gcm::aead::Payload { aad, msg })
                .ok()
                .map(|r| zeroize::Zeroizing::new(r.into()))
        }
    }

    pub fn emit_context(&self, encoder: &mut Encoder, source: &eid::Eid) {
        encoder.emit(&ContextId::BCB_AES_GCM);
        encoder.emit(&1);
        encoder.emit(source);
        encoder.emit(self.parameters.as_ref());
    }

    pub fn emit_result(&self, array: &mut Array) {
        array.emit(&self.results);
    }
}

pub fn parse(
    asb: asb::AbstractSyntaxBlock,
    data: &[u8],
) -> Result<(eid::Eid, HashMap<u64, bcb::Operation>), Error> {
    asb.into_operations(
        data,
        "RFC9173 AES-GCM parameters",
        "RFC9173 AES-GCM results",
        |r, d| Parameters::from_cbor(r, d).map_err(Error::from),
        |r, d| Results::from_cbor(r, d).map_err(Error::from),
        |parameters, results| {
            bcb::Operation::AES_GCM(Operation {
                parameters,
                results,
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::ops::Range;

    use aes_gcm::{AesGcm, aes::Aes256};

    use super::*;
    // `Error` in this file is `bpsec::Error`; the context leaf enum needs an
    // alias to be nameable alongside it.
    use crate::{HashMap, bpsec::context::Error as ContextError};

    // RFC 9173 §4.3.1: decrypt must accept any IV of 8-16 bytes, not only 12.
    // Encrypt with a given nonce size via the crate's own encrypt_inner, then
    // decrypt through the size-dispatching decrypt_gcm and check the round trip.
    #[test]
    fn decrypt_accepts_8_to_16_byte_iv() {
        let key = rand_bytes::<32>().unwrap();
        let aad: &[u8] = b"associated data";
        let plaintext: &[u8] = b"confidential payload";

        macro_rules! roundtrip {
            ($n:ty, $len:expr) => {{
                let iv = Iv::from_bytes(&rand_bytes::<$len>().unwrap()).unwrap();
                let cipher = AesGcm::<Aes256, $n>::new_from_slice(&key).unwrap();
                let ct = encrypt_inner(cipher, iv.as_slice(), aad, plaintext).unwrap();
                let (ciphertext, tag) = ct.split_at(ct.len() - 16);
                let op = Operation {
                    parameters: Arc::new(Parameters {
                        iv,
                        variant: AesVariant::A256GCM,
                        key: None,
                        flags: ScopeFlags::default(),
                    }),
                    results: Results(Some(tag.into())),
                };
                let out = op
                    .decrypt_gcm::<Aes256>(&key, aad, ciphertext)
                    .unwrap_or_else(|e| panic!("IV length {} should decrypt: {e}", $len));
                assert_eq!(out.as_ref(), plaintext, "IV length {} round trip", $len);
            }};
        }

        roundtrip!(U8, 8);
        roundtrip!(U9, 9);
        roundtrip!(U10, 10);
        roundtrip!(U11, 11);
        roundtrip!(U12, 12);
        roundtrip!(U13, 13);
        roundtrip!(U14, 14);
        roundtrip!(U15, 15);
        roundtrip!(U16, 16);
    }

    // Encode `bytes` as parameter 1 (the IV) of a definite-length CBOR byte
    // string, paired with the parameter map the decoder expects.
    fn iv_parameter(bytes: &[u8]) -> (HashMap<u64, Range<usize>>, Vec<u8>) {
        assert!(bytes.len() < 24, "single-byte byte-string head only");
        let mut data = Vec::with_capacity(bytes.len() + 1);
        data.push(0x40 | bytes.len() as u8);
        data.extend_from_slice(bytes);
        ([(1, 0..data.len())].into_iter().collect(), data)
    }

    // RFC 9173 §4.3.1: an IV outside the 8-16 byte range is rejected at
    // parse, the one remaining runtime check (`Iv::from_bytes`).
    #[test]
    fn parameters_reject_out_of_range_iv() {
        // 20 bytes overshoots the upper boundary.
        let (params, data) = iv_parameter(&rand_bytes::<20>().unwrap());
        assert!(matches!(
            Parameters::from_cbor(params, &data),
            Err(ContextError::InvalidIvLength(20))
        ));

        // 7 bytes undershoots the lower boundary.
        let (params, data) = iv_parameter(&rand_bytes::<7>().unwrap());
        assert!(matches!(
            Parameters::from_cbor(params, &data),
            Err(ContextError::InvalidIvLength(7))
        ));

        // 12 bytes is in range, and the IV survives the round trip intact.
        let iv = rand_bytes::<12>().unwrap();
        let (params, data) = iv_parameter(&iv);
        let parsed = Parameters::from_cbor(params, &data).unwrap();
        assert_eq!(parsed.iv.as_slice(), iv.as_ref());
    }
}
