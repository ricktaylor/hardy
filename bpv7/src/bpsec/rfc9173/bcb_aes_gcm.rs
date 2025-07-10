use super::*;
use aes_gcm::KeyInit;
use alloc::rc::Rc;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AesVariant {
    A128GCM,
    #[default]
    A256GCM,
    Unrecognised(u64),
}

impl hardy_cbor::encode::ToCbor for AesVariant {
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(match self {
            Self::A128GCM => &1,
            Self::A256GCM => &3,
            Self::Unrecognised(v) => v,
        })
    }
}

impl hardy_cbor::decode::FromCbor for AesVariant {
    type Error = hardy_cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
            o.map(|(value, shortest, len)| {
                (
                    match value {
                        1 => Self::A128GCM,
                        3 => Self::A256GCM,
                        v => Self::Unrecognised(v),
                    },
                    shortest,
                    len,
                )
            })
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Parameters {
    iv: Box<[u8]>,
    variant: AesVariant,
    key: Option<Box<[u8]>>,
    flags: ScopeFlags,
}

impl Parameters {
    fn from_cbor(
        parameters: HashMap<u64, Range<usize>>,
        data: &[u8],
    ) -> Result<(Self, bool), Error> {
        let mut shortest = true;
        let mut iv = None;
        let mut variant = None;
        let mut key = None;
        let mut flags = None;
        for (id, range) in parameters {
            match id {
                1 => {
                    iv = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                2 => {
                    variant = Some(
                        hardy_cbor::decode::parse(&data[range.start..range.end]).map(
                            |(v, s)| {
                                shortest = shortest && s;
                                v
                            },
                        )?,
                    );
                }
                3 => {
                    key = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                4 => {
                    flags = Some(
                        hardy_cbor::decode::parse(&data[range.start..range.end]).map(
                            |(v, s)| {
                                shortest = shortest && s;
                                v
                            },
                        )?,
                    );
                }
                _ => return Err(Error::InvalidContextParameter(id)),
            }
        }
        let Some(iv) = iv else {
            return Err(Error::MissingContextParameter(1));
        };

        Ok((
            Self {
                iv,
                variant: variant.unwrap_or_default(),
                key,
                flags: flags.unwrap_or_default(),
            },
            shortest,
        ))
    }
}

impl hardy_cbor::encode::ToCbor for Parameters {
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
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
                    a.emit_array(Some(2), |a| {
                        a.emit(&b);
                        match b {
                            1 => a.emit(self.iv.as_ref()),
                            2 => a.emit(&self.variant),
                            3 => a.emit(self.key.as_ref().unwrap().as_ref()),
                            4 => a.emit(&self.flags),
                            _ => unreachable!(),
                        };
                    });
                }
            }
        })
    }
}

#[derive(Debug)]
struct Results(Option<Box<[u8]>>);

impl Results {
    fn from_cbor(results: HashMap<u64, Range<usize>>, data: &[u8]) -> Result<(Self, bool), Error> {
        let mut shortest = true;
        let mut r = None;
        for (id, range) in results {
            match id {
                1 => {
                    r = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                _ => return Err(Error::InvalidContextResult(id)),
            }
        }

        Ok((Self(r), shortest))
    }
}

impl hardy_cbor::encode::ToCbor for Results {
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        if let Some(r) = self.0.as_ref() {
            encoder.emit_array(Some(1), |a| {
                a.emit_array(Some(2), |a| {
                    a.emit(&1);
                    a.emit(r.as_ref());
                });
            })
        } else {
            encoder.emit_array(Some(0), |_| {})
        }
    }
}

fn build_data(
    flags: &ScopeFlags,
    args: &bcb::OperationArgs,
    payload_data: Option<&[u8]>,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut encoder = hardy_cbor::encode::Encoder::new();
    encoder.emit(&ScopeFlags {
        include_primary_block: flags.include_primary_block,
        include_target_header: flags.include_target_header,
        include_security_header: flags.include_security_header,
        ..Default::default()
    });

    if flags.include_primary_block {
        encoder.emit_raw_slice(args.primary_block);
    }

    if flags.include_target_header {
        encoder.emit(&args.target.block_type);
        encoder.emit(&args.target_number);
        encoder.emit(&args.target.flags);
    }

    if flags.include_security_header {
        encoder.emit(&args.source.block_type);
        encoder.emit(&args.source_number);
        encoder.emit(&args.source.flags);
    }
    let aad = encoder.build();

    let data = if let Some(payload_data) = payload_data {
        payload_data.into()
    } else {
        hardy_cbor::decode::parse_value(args.target_payload, |value, _, _| {
            match value {
                hardy_cbor::decode::Value::ByteStream(data) => {
                    // Concatenate all the bytes
                    Ok::<_, Error>(data.iter().fold(Vec::new(), |mut data, d| {
                        data.extend(*d);
                        data
                    }))
                }
                hardy_cbor::decode::Value::Bytes(data) => Ok(data.into()),
                _ => unreachable!(),
            }
        })
        .map(|v| v.0)?
    };

    Ok((data, aad))
}

#[allow(clippy::type_complexity)]
fn encrypt_inner(
    cipher: impl aes_gcm::aead::AeadInPlace,
    aad: Vec<u8>,
    mut data: Vec<u8>,
) -> Result<(Box<[u8]>, Box<[u8]>), Error> {
    // Generate IV
    let mut iv = [0u8; 12];
    OsRng
        .try_fill_bytes(&mut iv)
        .map_err(|e| Error::Algorithm(e.into()))?;

    // Encrypt in-place, this results in a single data copy
    cipher
        .encrypt_in_place(iv.as_ref().into(), &aad, &mut data)
        .map(|_| (data.into(), iv.into()))
        .map_err(|_| Error::EncryptionFailed)
}

#[derive(Debug)]
pub struct Operation {
    parameters: Rc<Parameters>,
    results: Results,
}

impl Operation {
    pub fn is_unsupported(&self) -> bool {
        matches!(self.parameters.variant, AesVariant::Unrecognised(_))
    }

    #[allow(clippy::type_complexity)]
    pub fn encrypt(
        jwk: &Key,
        args: bcb::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<Option<(Self, Box<[u8]>)>, Error> {
        if let Some(ops) = &jwk.operations {
            if !ops.contains(&key::Operation::Encrypt) {
                return Ok(None);
            }
        }

        let (cek, variant) = match &jwk.key_algorithm {
            Some(key::KeyAlgorithm::A128KW)
            | Some(key::KeyAlgorithm::A192KW)
            | Some(key::KeyAlgorithm::A256KW) => {
                if let Some(ops) = &jwk.operations {
                    if !ops.contains(&key::Operation::WrapKey) {
                        return Ok(None);
                    }
                }
                match &jwk.enc_algorithm {
                    Some(key::EncAlgorithm::A128GCM) => {
                        (Some(rand_key(Box::from([0u8; 32]))?), AesVariant::A128GCM)
                    }
                    None | Some(key::EncAlgorithm::A256GCM) => {
                        (Some(rand_key(Box::from([0u8; 64]))?), AesVariant::A256GCM)
                    }
                    _ => return Ok(None),
                }
            }
            Some(key::KeyAlgorithm::Direct) | None => (
                None,
                match &jwk.enc_algorithm {
                    Some(key::EncAlgorithm::A128GCM) => AesVariant::A128GCM,
                    None | Some(key::EncAlgorithm::A256GCM) => AesVariant::A256GCM,
                    _ => return Ok(None),
                },
            ),
            _ => {
                return Ok(None);
            }
        };

        let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
            return Ok(None);
        };

        let flags = ScopeFlags::default();
        let (data, aad) = build_data(&flags, &args, payload_data)?;
        if let Some(cek) = cek {
            let cek = match &jwk.key_algorithm {
                Some(key::KeyAlgorithm::A128KW) => aes_kw::KekAes128::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.into())),
                Some(key::KeyAlgorithm::A192KW) => aes_kw::KekAes192::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.into())),
                Some(key::KeyAlgorithm::A256KW) => aes_kw::KekAes256::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.into())),
                _ => unreachable!(),
            }?;

            let (ciphertext, iv) = match variant {
                AesVariant::A128GCM => aes_gcm::Aes128Gcm::new_from_slice(&cek)
                    .map_err(|e| Error::Algorithm(e.into()))
                    .and_then(|cipher| encrypt_inner(cipher, aad, data)),
                AesVariant::A256GCM => aes_gcm::Aes256Gcm::new_from_slice(&cek)
                    .map_err(|e| Error::Algorithm(e.into()))
                    .and_then(|cipher| encrypt_inner(cipher, aad, data)),
                AesVariant::Unrecognised(_) => unreachable!(),
            }?;

            Ok(Some((
                Self {
                    parameters: Rc::new(Parameters {
                        iv,
                        variant,
                        key: Some(cek.into()),
                        flags,
                    }),
                    results: Results(None),
                },
                ciphertext,
            )))
        } else {
            let (ciphertext, iv) = match variant {
                AesVariant::A128GCM => aes_gcm::Aes128Gcm::new_from_slice(kek)
                    .map_err(|e| Error::Algorithm(e.into()))
                    .and_then(|cipher| encrypt_inner(cipher, aad, data)),
                AesVariant::A256GCM => aes_gcm::Aes256Gcm::new_from_slice(kek)
                    .map_err(|e| Error::Algorithm(e.into()))
                    .and_then(|cipher| encrypt_inner(cipher, aad, data)),
                AesVariant::Unrecognised(_) => unreachable!(),
            }?;

            Ok(Some((
                Self {
                    parameters: Rc::new(Parameters {
                        iv,
                        variant,
                        key: None,
                        flags,
                    }),
                    results: Results(None),
                },
                ciphertext,
            )))
        }
    }

    pub fn decrypt_any(
        &self,
        key_f: &impl key::KeyStore,
        args: bcb::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<bcb::DecryptResult, Error> {
        if let Some(cek) = &self.parameters.key {
            for jwk in key_f.decrypt_keys(
                args.bpsec_source,
                &[key::Operation::UnwrapKey, key::Operation::Decrypt],
            ) {
                if let key::Type::OctetSequence { key: kek } = &jwk.key_type {
                    if let Some(cek) = match &jwk.key_algorithm {
                        Some(key::KeyAlgorithm::A128KW) => {
                            aes_kw::KekAes128::try_from(kek.as_ref())
                                .and_then(|kek| kek.unwrap_vec(cek))
                                .ok()
                        }
                        Some(key::KeyAlgorithm::A192KW) => {
                            aes_kw::KekAes192::try_from(kek.as_ref())
                                .and_then(|kek| kek.unwrap_vec(cek))
                                .ok()
                        }
                        Some(key::KeyAlgorithm::A256KW) => {
                            aes_kw::KekAes256::try_from(kek.as_ref())
                                .and_then(|kek| kek.unwrap_vec(cek))
                                .ok()
                        }
                        _ => None,
                    }
                    .map(|v| zeroize::Zeroizing::from(Box::from(v)))
                    {
                        if let Some(plaintext) = match (self.parameters.variant, &jwk.enc_algorithm)
                        {
                            (AesVariant::A128GCM, Some(key::EncAlgorithm::A128GCM) | None) => {
                                let (data, aad) =
                                    build_data(&self.parameters.flags, &args, payload_data)?;
                                aes_gcm::Aes128Gcm::new_from_slice(&cek)
                                    .ok()
                                    .and_then(|cek| self.decrypt_inner(cek, &aad, data).ok())
                            }
                            (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                                let (data, aad) =
                                    build_data(&self.parameters.flags, &args, payload_data)?;
                                aes_gcm::Aes256Gcm::new_from_slice(&cek)
                                    .ok()
                                    .and_then(|cek| self.decrypt_inner(cek, &aad, data).ok())
                            }
                            (AesVariant::Unrecognised(_), _) => {
                                return Err(Error::UnsupportedOperation);
                            }
                            _ => None,
                        } {
                            return Ok(bcb::DecryptResult {
                                plaintext: Some(plaintext),
                                protects_primary_block: self.parameters.flags.include_primary_block,
                            });
                        }
                    }
                }
            }
        } else {
            for jwk in key_f.decrypt_keys(args.bpsec_source, &[key::Operation::Decrypt]) {
                if let Some(key_algorithm) = &jwk.key_algorithm {
                    if !matches!(key_algorithm, key::KeyAlgorithm::Direct) {
                        continue;
                    }
                };

                if let key::Type::OctetSequence { key: cek } = &jwk.key_type {
                    if let Some(plaintext) = match (self.parameters.variant, &jwk.enc_algorithm) {
                        (AesVariant::A128GCM, Some(key::EncAlgorithm::A128GCM) | None) => {
                            let (data, aad) =
                                build_data(&self.parameters.flags, &args, payload_data)?;
                            aes_gcm::Aes128Gcm::new_from_slice(cek)
                                .ok()
                                .and_then(|cek| self.decrypt_inner(cek, &aad, data).ok())
                        }
                        (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                            let (data, aad) =
                                build_data(&self.parameters.flags, &args, payload_data)?;
                            aes_gcm::Aes256Gcm::new_from_slice(cek)
                                .ok()
                                .and_then(|cek| self.decrypt_inner(cek, &aad, data).ok())
                        }
                        (AesVariant::Unrecognised(_), _) => {
                            return Err(Error::UnsupportedOperation);
                        }
                        _ => None,
                    } {
                        return Ok(bcb::DecryptResult {
                            plaintext: Some(plaintext),
                            protects_primary_block: self.parameters.flags.include_primary_block,
                        });
                    }
                }
            }
        }

        Ok(bcb::DecryptResult {
            plaintext: None,
            protects_primary_block: self.parameters.flags.include_primary_block,
        })
    }

    pub fn decrypt(
        &self,
        jwk: &Key,
        args: bcb::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<bcb::DecryptResult, Error> {
        let (data, aad) = build_data(&self.parameters.flags, &args, payload_data)?;

        if let Some(cek) = &self.parameters.key {
            let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
                return Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone()));
            };

            let cek = match &jwk.key_algorithm {
                Some(key::KeyAlgorithm::A128KW) => aes_kw::KekAes128::try_from(kek.as_ref())
                    .and_then(|kek| kek.unwrap_vec(cek))
                    .map_err(|e| Error::Algorithm(e.into())),
                Some(key::KeyAlgorithm::A192KW) => aes_kw::KekAes192::try_from(kek.as_ref())
                    .and_then(|kek| kek.unwrap_vec(cek))
                    .map_err(|e| Error::Algorithm(e.into())),
                Some(key::KeyAlgorithm::A256KW) => aes_kw::KekAes256::try_from(kek.as_ref())
                    .and_then(|kek| kek.unwrap_vec(cek))
                    .map_err(|e| Error::Algorithm(e.into())),
                _ => Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone())),
            }
            .map(|v| zeroize::Zeroizing::from(Box::from(v)))?;

            match (self.parameters.variant, &jwk.enc_algorithm) {
                (AesVariant::A128GCM, Some(key::EncAlgorithm::A128GCM) | None) => {
                    aes_gcm::Aes128Gcm::new_from_slice(&cek)
                        .map_err(|e| Error::Algorithm(e.into()))
                        .and_then(|cek| self.decrypt_inner(cek, &aad, data))
                }
                (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                    aes_gcm::Aes256Gcm::new_from_slice(&cek)
                        .map_err(|e| Error::Algorithm(e.into()))
                        .and_then(|cek| self.decrypt_inner(cek, &aad, data))
                }
                _ => Err(Error::UnsupportedOperation),
            }
        } else {
            let key::Type::OctetSequence { key: cek } = &jwk.key_type else {
                return Err(Error::InvalidKey(key::Operation::Decrypt, jwk.clone()));
            };

            match (self.parameters.variant, &jwk.enc_algorithm) {
                (AesVariant::A128GCM, Some(key::EncAlgorithm::A128GCM) | None) => {
                    aes_gcm::Aes128Gcm::new_from_slice(cek)
                        .map_err(|e| Error::Algorithm(e.into()))
                        .and_then(|cek| self.decrypt_inner(cek, &aad, data))
                }
                (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                    aes_gcm::Aes256Gcm::new_from_slice(cek)
                        .map_err(|e| Error::Algorithm(e.into()))
                        .and_then(|cek| self.decrypt_inner(cek, &aad, data))
                }
                (AesVariant::Unrecognised(_), _) => Err(Error::UnsupportedOperation),
                _ => Err(Error::InvalidKey(key::Operation::Decrypt, jwk.clone())),
            }
        }
        .map(|plaintext| bcb::DecryptResult {
            plaintext: Some(plaintext),
            protects_primary_block: self.parameters.flags.include_primary_block,
        })
    }

    fn decrypt_inner(
        &self,
        cipher: impl aes_gcm::aead::AeadInPlace,
        aad: &[u8],
        mut data: Vec<u8>,
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error> {
        // Decrypt in-place, this results in a single data copy
        if let Some(tag) = self.results.0.as_ref() {
            cipher.decrypt_in_place_detached(
                self.parameters.iv.as_ref().into(),
                aad,
                &mut data,
                tag.as_ref().into(),
            )
        } else {
            cipher.decrypt_in_place(self.parameters.iv.as_ref().into(), aad, &mut data)
        }
        .map(|()| zeroize::Zeroizing::new(data.into()))
        .map_err(|_| Error::DecryptionFailed)
    }

    pub fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
        encoder.emit(&Context::BIB_HMAC_SHA2);
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
) -> Result<(eid::Eid, HashMap<u64, bcb::Operation>, bool), Error> {
    let mut shortest = false;
    let parameters = Rc::from(
        Parameters::from_cbor(asb.parameters, data)
            .map(|(p, s)| {
                shortest = s;
                p
            })
            .map_field_err("RFC9173 AES-GCM parameters")?,
    );

    // Unpack results
    let mut operations = HashMap::new();
    for (target, results) in asb.results {
        operations.insert(
            target,
            bcb::Operation::AES_GCM(Operation {
                parameters: parameters.clone(),
                results: Results::from_cbor(results, data)
                    .map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })
                    .map_field_err("RFC9173 AES-GCM results")?,
            }),
        );
    }
    Ok((asb.source, operations, shortest))
}
