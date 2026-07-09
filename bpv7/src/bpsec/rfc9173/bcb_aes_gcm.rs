use alloc::rc::Rc;

use aes_gcm::{
    KeyInit,
    aes::cipher::consts::{U8, U9, U10, U11, U12, U13, U14, U15, U16},
};

use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AesVariant {
    A128GCM,
    #[default]
    A256GCM,
    Unrecognised(u64),
}

impl hardy_cbor::encode::ToCbor for AesVariant {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        match self {
            Self::A128GCM => encoder.emit(&1),
            Self::A256GCM => encoder.emit(&3),
            Self::Unrecognised(v) => encoder.emit(v),
        }
    }
}

impl hardy_cbor::decode::FromCbor for AesVariant {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, shortest, len) =
            hardy_cbor::decode::parse::<(u64, bool, usize)>(data).map_err(Error::InvalidCBOR)?;
        if !shortest {
            return Err(Error::NotCanonical);
        }
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
    pub iv: Box<[u8]>,
    pub variant: AesVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: ScopeFlags,
}

impl Parameters {
    fn from_cbor(parameters: HashMap<u64, Range<usize>>, data: &[u8]) -> Result<Self, Error> {
        let mut iv = None;
        let mut variant = None;
        let mut key = None;
        let mut flags = None;
        for (id, range) in parameters {
            match id {
                1 => iv = Some(parse::decode_box(range, data)?),
                2 => variant = Some(hardy_cbor::decode::parse(&data[range])?),
                3 => key = Some(parse::decode_box(range, data)?),
                4 => flags = Some(hardy_cbor::decode::parse(&data[range])?),
                _ => return Err(Error::InvalidContextParameter(id)),
            }
        }

        // RFC 9173 §4.3.1: the IV length MUST be between 8 and 16 bytes.
        let iv = iv.ok_or(Error::MissingContextParameter(1))?;
        if !(8..=16).contains(&iv.len()) {
            return Err(Error::InvalidIvLength(iv.len()));
        }

        Ok(Self {
            iv,
            variant: variant.unwrap_or_default(),
            key,
            flags: flags.unwrap_or_default(),
        })
    }
}

impl hardy_cbor::encode::ToCbor for Parameters {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
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
                        1 => a.emit(&(b, &hardy_cbor::encode::Bytes(&self.iv))),
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
    fn from_cbor(results: HashMap<u64, Range<usize>>, data: &[u8]) -> Result<Self, Error> {
        let mut r = None;
        for (id, range) in results {
            match id {
                1 => r = Some(parse::decode_box(range, data)?),
                _ => return Err(Error::InvalidContextResult(id)),
            }
        }

        Ok(Self(r))
    }
}

impl hardy_cbor::encode::ToCbor for Results {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        if let Some(r) = self.0.as_ref() {
            encoder.emit(&[&(1, &hardy_cbor::encode::Bytes(r))]);
        } else {
            encoder.emit::<[u8; 0]>(&[])
        }
    }
}

fn build_data(flags: &ScopeFlags, args: &bcb::OperationArgs) -> Result<Vec<u8>, Error> {
    let mut encoder = hardy_cbor::encode::Encoder::new();
    encoder.emit(&ScopeFlags {
        include_primary_block: flags.include_primary_block,
        include_target_header: flags.include_target_header,
        include_security_header: flags.include_security_header,
        ..Default::default()
    });

    if flags.include_primary_block {
        encoder.emit(&hardy_cbor::encode::Raw(
            args.blocks
                .block(0)
                .and_then(|v| v.1)
                .expect("Missing primary block!")
                .as_ref(),
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

#[allow(clippy::type_complexity)]
fn encrypt_inner<C: aes_gcm::aead::Aead>(
    cipher: C,
    iv: Box<[u8]>,
    aad: &[u8],
    msg: &[u8],
) -> Result<(Box<[u8]>, Box<[u8]>), Error> {
    let nonce =
        <&aes_gcm::aead::Nonce<C>>::try_from(iv.as_ref()).map_err(|_| Error::EncryptionFailed)?;
    cipher
        .encrypt(nonce, aes_gcm::aead::Payload { msg, aad })
        .map(|r| (r.into(), iv))
        .map_err(|_| Error::EncryptionFailed)
}

#[derive(Debug)]
pub struct Operation {
    pub parameters: Rc<Parameters>,
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

        let (cek, variant) = match &jwk.key_algorithm {
            Some(key::KeyAlgorithm::A128KW)
            | Some(key::KeyAlgorithm::A192KW)
            | Some(key::KeyAlgorithm::A256KW) => {
                if let Some(ops) = &jwk.operations
                    && !ops.contains(&key::Operation::WrapKey)
                {
                    return Err(Error::InvalidKey(key::Operation::WrapKey, jwk.clone()));
                }
                match &jwk.enc_algorithm {
                    Some(key::EncAlgorithm::A128GCM) => (
                        Some(zeroize::Zeroizing::from(rand_bytes::<16>()?)),
                        AesVariant::A128GCM,
                    ),
                    None | Some(key::EncAlgorithm::A256GCM) => (
                        Some(zeroize::Zeroizing::from(rand_bytes::<32>()?)),
                        AesVariant::A256GCM,
                    ),
                    _ => return Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone())),
                }
            }
            Some(key::KeyAlgorithm::Direct) | None => (
                None,
                match &jwk.enc_algorithm {
                    Some(key::EncAlgorithm::A128GCM) => AesVariant::A128GCM,
                    None | Some(key::EncAlgorithm::A256GCM) => AesVariant::A256GCM,
                    _ => return Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone())),
                },
            ),
            _ => {
                return Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone()));
            }
        };

        let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
            return Err(Error::InvalidKey(key::Operation::Encrypt, jwk.clone()));
        };

        let aad = build_data(&scope_flags, &args)?;

        let active_cek = cek
            .as_ref()
            .map_or(kek.as_ref(), |cek: &zeroize::Zeroizing<Box<[u8]>>| {
                cek.as_ref()
            });

        let (ciphertext, iv) = match variant {
            AesVariant::A128GCM => aes_gcm::Aes128Gcm::new_from_slice(active_cek)
                .map_err(|e| Error::Algorithm(e.to_string()))
                .and_then(|cipher| {
                    encrypt_inner(cipher, rand_bytes::<12>()?, &aad, payload.as_ref())
                }),
            AesVariant::A256GCM => aes_gcm::Aes256Gcm::new_from_slice(active_cek)
                .map_err(|e| Error::Algorithm(e.to_string()))
                .and_then(|cipher| {
                    encrypt_inner(cipher, rand_bytes::<12>()?, &aad, payload.as_ref())
                }),
            AesVariant::Unrecognised(_) => {
                unreachable!("Unrecognised variants filtered before encryption")
            }
        }?;

        let key = if let Some(cek) = cek {
            Some(
                match &jwk.key_algorithm {
                    Some(key::KeyAlgorithm::A128KW) => {
                        key_wrap::wrap::<aes_kw::aes::Aes128>(kek.as_ref(), &cek)
                    }
                    Some(key::KeyAlgorithm::A192KW) => {
                        key_wrap::wrap::<aes_kw::aes::Aes192>(kek.as_ref(), &cek)
                    }
                    Some(key::KeyAlgorithm::A256KW) => {
                        key_wrap::wrap::<aes_kw::aes::Aes256>(kek.as_ref(), &cek)
                    }
                    _ => unreachable!("Key algorithm validated during key lookup"),
                }
                .map_err(Error::Algorithm)?
                .into(),
            )
        } else {
            None
        };

        Ok((
            Self {
                parameters: Rc::new(Parameters {
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

            let cek = match &jwk.key_algorithm {
                Some(key::KeyAlgorithm::A128KW) => {
                    key_wrap::unwrap::<aes_kw::aes::Aes128>(kek.as_ref(), wrapped_cek)
                }
                Some(key::KeyAlgorithm::A192KW) => {
                    key_wrap::unwrap::<aes_kw::aes::Aes192>(kek.as_ref(), wrapped_cek)
                }
                Some(key::KeyAlgorithm::A256KW) => {
                    key_wrap::unwrap::<aes_kw::aes::Aes256>(kek.as_ref(), wrapped_cek)
                }
                _ => return Err(Error::DecryptionFailed),
            }
            .map_err(|_| Error::DecryptionFailed)?;
            let cek = zeroize::Zeroizing::from(Box::<[u8]>::from(cek));

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

            self.decrypt_middle(jwk.enc_algorithm, cek.as_ref(), &aad, data.as_ref())
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
            (AesVariant::Unrecognised(_), _) => Err(Error::UnsupportedOperation),
            _ => Err(Error::DecryptionFailed),
        }
    }

    // AES-GCM decryption dispatched on the wire IV length. RFC 9173 §4.3.1
    // permits any IV of 8-16 bytes; aes-gcm parameterises the cipher by its
    // nonce size, so the runtime length is matched to the corresponding
    // `AesGcm<Aes, Un>` type (decrypt_inner is generic over the cipher). Encrypt
    // always emits 12-byte IVs (the RFC's SHOULD); this only widens acceptance
    // on decrypt. Parameters::from_cbor already bounds the length to 8-16, so
    // the fallthrough is defensive.
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
            ($n:ty) => {{
                let cipher = aes_gcm::AesGcm::<Aes, $n>::new_from_slice(cek)
                    .map_err(|_| Error::DecryptionFailed)?;
                self.decrypt_inner(cipher, aad, data)
                    .ok_or(Error::DecryptionFailed)
            }};
        }

        match self.parameters.iv.len() {
            8 => decrypt_sized!(U8),
            9 => decrypt_sized!(U9),
            10 => decrypt_sized!(U10),
            11 => decrypt_sized!(U11),
            12 => decrypt_sized!(U12),
            13 => decrypt_sized!(U13),
            14 => decrypt_sized!(U14),
            15 => decrypt_sized!(U15),
            16 => decrypt_sized!(U16),
            n => Err(Error::InvalidIvLength(n)),
        }
    }

    fn decrypt_inner<C: aes_gcm::aead::Aead + aes_gcm::aead::AeadInOut>(
        &self,
        cipher: C,
        aad: &[u8],
        msg: &[u8],
    ) -> Option<zeroize::Zeroizing<Box<[u8]>>> {
        let nonce = <&aes_gcm::aead::Nonce<C>>::try_from(self.parameters.iv.as_ref()).ok()?;
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

    pub fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
        encoder.emit(&Context::BCB_AES_GCM);
        encoder.emit(&1);
        encoder.emit(source);
        encoder.emit(self.parameters.as_ref());
    }

    pub fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
        array.emit(&self.results);
    }
}

pub fn parse(
    asb: parse::AbstractSyntaxBlock,
    data: &[u8],
) -> Result<(eid::Eid, HashMap<u64, bcb::Operation>), Error> {
    let parameters = Rc::from(
        Parameters::from_cbor(asb.parameters, data)
            .map_field_err::<Error>("RFC9173 AES-GCM parameters")?,
    );

    // Unpack results
    let mut operations = HashMap::with_capacity(asb.results.len());
    for (target, results) in asb.results {
        operations.insert(
            target,
            bcb::Operation::AES_GCM(Operation {
                parameters: parameters.clone(),
                results: Results::from_cbor(results, data)
                    .map_field_err::<Error>("RFC9173 AES-GCM results")?,
            }),
        );
    }
    Ok((asb.source, operations))
}

#[cfg(test)]
mod tests {
    use aes_gcm::KeyInit;
    use alloc::rc::Rc;
    use core::ops::Range;

    use super::*;
    use crate::HashMap;
    use crate::bpsec::Error;
    use crate::bpsec::rfc9173::ScopeFlags;

    // RFC 9173 §4.3.1: decrypt must accept any IV of 8-16 bytes, not only 12.
    // Encrypt with a given nonce size via the crate's own encrypt_inner, then
    // decrypt through the size-dispatching decrypt_gcm and check the round trip.
    #[test]
    fn decrypt_accepts_8_to_16_byte_iv() {
        let key = [0x42u8; 32];
        let aad: &[u8] = b"associated data";
        let plaintext: &[u8] = b"confidential payload";

        macro_rules! roundtrip {
            ($n:ty, $len:expr) => {{
                let iv: Box<[u8]> = alloc::vec![0xAB; $len].into();
                let cipher =
                    aes_gcm::AesGcm::<aes_gcm::aes::Aes256, $n>::new_from_slice(&key).unwrap();
                let (ct, _) = encrypt_inner(cipher, iv.clone(), aad, plaintext).unwrap();
                let (ciphertext, tag) = ct.split_at(ct.len() - 16);
                let op = Operation {
                    parameters: Rc::new(Parameters {
                        iv,
                        variant: AesVariant::A256GCM,
                        key: None,
                        flags: ScopeFlags::default(),
                    }),
                    results: Results(Some(tag.into())),
                };
                let out = op
                    .decrypt_gcm::<aes_gcm::aes::Aes256>(&key, aad, ciphertext)
                    .unwrap_or_else(|e| panic!("IV length {} should decrypt: {e}", $len));
                assert_eq!(out.as_ref(), plaintext, "IV length {} round trip", $len);
            }};
        }

        roundtrip!(aes_gcm::aes::cipher::consts::U8, 8);
        roundtrip!(aes_gcm::aes::cipher::consts::U12, 12);
        roundtrip!(aes_gcm::aes::cipher::consts::U16, 16);
    }

    // RFC 9173 §4.3.1: an IV outside the 8-16 byte range is rejected at parse.
    #[test]
    fn parameters_reject_out_of_range_iv() {
        // Parameter 1 (IV) as a 20-byte CBOR byte string: 0x54 head + 20 bytes.
        let mut data = alloc::vec![0x54u8];
        data.extend_from_slice(&[0u8; 20]);
        let params: HashMap<u64, Range<usize>> = [(1, 0..data.len())].into_iter().collect();
        assert!(matches!(
            Parameters::from_cbor(params, &data),
            Err(Error::InvalidIvLength(20))
        ));

        // A 12-byte IV (0x4C head + 12 bytes) is accepted.
        let mut data = alloc::vec![0x4Cu8];
        data.extend_from_slice(&[0u8; 12]);
        let params: HashMap<u64, Range<usize>> = [(1, 0..data.len())].into_iter().collect();
        assert!(Parameters::from_cbor(params, &data).is_ok());
    }
}
