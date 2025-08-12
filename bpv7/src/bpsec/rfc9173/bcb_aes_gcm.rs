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

        Ok((
            Self {
                iv: iv.ok_or(Error::MissingContextParameter(1))?,
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

fn build_data<'a>(
    flags: &ScopeFlags,
    args: &'a bcb::OperationArgs,
) -> Result<(Vec<u8>, &'a [u8]), Error> {
    let data = args
        .blocks
        .block_payload(args.target)
        .ok_or(Error::MissingSecurityTarget)?;

    let mut encoder = hardy_cbor::encode::Encoder::new();
    encoder.emit(&ScopeFlags {
        include_primary_block: flags.include_primary_block,
        include_target_header: flags.include_target_header,
        include_security_header: flags.include_security_header,
        ..Default::default()
    });

    if flags.include_primary_block {
        encoder.emit_raw_slice(
            args.blocks
                .block_payload(args.target)
                .expect("Missing primary block!"),
        );
    }

    if flags.include_target_header {
        let target_block = args
            .blocks
            .block(args.target)
            .ok_or(Error::MissingSecurityTarget)?;

        encoder.emit(&target_block.block_type);
        encoder.emit(&args.target);
        encoder.emit(&target_block.flags);
    }

    if flags.include_security_header {
        let source_block = args
            .blocks
            .block(args.source)
            .ok_or(Error::MissingSecurityTarget)?;

        encoder.emit(&source_block.block_type);
        encoder.emit(&args.source);
        encoder.emit(&source_block.flags);
    }

    Ok((encoder.build(), data))
}

#[allow(clippy::type_complexity)]
fn encrypt_inner(
    cipher: impl aes_gcm::aead::Aead,
    aad: &[u8],
    msg: &[u8],
) -> Result<(Box<[u8]>, Box<[u8]>), Error> {
    // Generate IV
    let mut iv = [0u8; 12];
    OsRng
        .try_fill_bytes(&mut iv)
        .map_err(|e| Error::Algorithm(e.to_string()))?;

    cipher
        .encrypt(iv.as_ref().into(), aes_gcm::aead::Payload { msg, aad })
        .map(|r| (r.into(), iv.into()))
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

    pub fn protects_primary_block(&self) -> bool {
        self.parameters.flags.include_primary_block
    }

    #[allow(clippy::type_complexity)]
    pub fn encrypt(
        jwk: &Key,
        args: bcb::OperationArgs,
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
        let (aad, data) = build_data(&flags, &args)?;

        if let Some(cek) = cek {
            let cek = match &jwk.key_algorithm {
                Some(key::KeyAlgorithm::A128KW) => aes_kw::KekAes128::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
                Some(key::KeyAlgorithm::A192KW) => aes_kw::KekAes192::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
                Some(key::KeyAlgorithm::A256KW) => aes_kw::KekAes256::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
                _ => unreachable!(),
            }?;

            let (ciphertext, iv) = match variant {
                AesVariant::A128GCM => aes_gcm::Aes128Gcm::new_from_slice(&cek)
                    .map_err(|e| Error::Algorithm(e.to_string()))
                    .and_then(|cipher| encrypt_inner(cipher, &aad, data)),
                AesVariant::A256GCM => aes_gcm::Aes256Gcm::new_from_slice(&cek)
                    .map_err(|e| Error::Algorithm(e.to_string()))
                    .and_then(|cipher| encrypt_inner(cipher, &aad, data)),
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
                    .map_err(|e| Error::Algorithm(e.to_string()))
                    .and_then(|cipher| encrypt_inner(cipher, &aad, data)),
                AesVariant::A256GCM => aes_gcm::Aes256Gcm::new_from_slice(kek)
                    .map_err(|e| Error::Algorithm(e.to_string()))
                    .and_then(|cipher| encrypt_inner(cipher, &aad, data)),
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
    ) -> Result<Option<zeroize::Zeroizing<Box<[u8]>>>, Error> {
        let (aad, data) = build_data(&self.parameters.flags, &args)?;

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
                                aes_gcm::Aes128Gcm::new_from_slice(&cek)
                                    .ok()
                                    .and_then(|cek| self.decrypt_inner(cek, &aad, data).ok())
                            }
                            (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                                aes_gcm::Aes256Gcm::new_from_slice(&cek)
                                    .ok()
                                    .and_then(|cek| self.decrypt_inner(cek, &aad, data).ok())
                            }
                            (AesVariant::Unrecognised(_), _) => {
                                return Err(Error::UnsupportedOperation);
                            }
                            _ => None,
                        } {
                            return Ok(Some(plaintext));
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
                            aes_gcm::Aes128Gcm::new_from_slice(cek)
                                .ok()
                                .and_then(|cek| self.decrypt_inner(cek, &aad, data).ok())
                        }
                        (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                            aes_gcm::Aes256Gcm::new_from_slice(cek)
                                .ok()
                                .and_then(|cek| self.decrypt_inner(cek, &aad, data).ok())
                        }
                        (AesVariant::Unrecognised(_), _) => {
                            return Err(Error::UnsupportedOperation);
                        }
                        _ => None,
                    } {
                        return Ok(Some(plaintext));
                    }
                }
            }
        }

        Ok(None)
    }

    pub fn decrypt(
        &self,
        jwk: &Key,
        args: bcb::OperationArgs,
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error> {
        let (aad, data) = build_data(&self.parameters.flags, &args)?;

        if let Some(cek) = &self.parameters.key {
            let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
                return Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone()));
            };

            let cek = match &jwk.key_algorithm {
                Some(key::KeyAlgorithm::A128KW) => aes_kw::KekAes128::try_from(kek.as_ref())
                    .and_then(|kek| kek.unwrap_vec(cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
                Some(key::KeyAlgorithm::A192KW) => aes_kw::KekAes192::try_from(kek.as_ref())
                    .and_then(|kek| kek.unwrap_vec(cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
                Some(key::KeyAlgorithm::A256KW) => aes_kw::KekAes256::try_from(kek.as_ref())
                    .and_then(|kek| kek.unwrap_vec(cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
                _ => Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone())),
            }
            .map(|v| zeroize::Zeroizing::from(Box::from(v)))?;

            match (self.parameters.variant, &jwk.enc_algorithm) {
                (AesVariant::A128GCM, Some(key::EncAlgorithm::A128GCM) | None) => {
                    aes_gcm::Aes128Gcm::new_from_slice(&cek)
                        .map_err(|e| Error::Algorithm(e.to_string()))
                        .and_then(|cek| self.decrypt_inner(cek, &aad, data))
                }
                (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                    aes_gcm::Aes256Gcm::new_from_slice(&cek)
                        .map_err(|e| Error::Algorithm(e.to_string()))
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
                        .map_err(|e| Error::Algorithm(e.to_string()))
                        .and_then(|cek| self.decrypt_inner(cek, &aad, data))
                }
                (AesVariant::A256GCM, Some(key::EncAlgorithm::A256GCM) | None) => {
                    aes_gcm::Aes256Gcm::new_from_slice(cek)
                        .map_err(|e| Error::Algorithm(e.to_string()))
                        .and_then(|cek| self.decrypt_inner(cek, &aad, data))
                }
                (AesVariant::Unrecognised(_), _) => Err(Error::UnsupportedOperation),
                _ => Err(Error::InvalidKey(key::Operation::Decrypt, jwk.clone())),
            }
        }
    }

    fn decrypt_inner<C: aes_gcm::aead::Aead + aes_gcm::aead::AeadInPlace>(
        &self,
        cipher: C,
        aad: &[u8],
        msg: &[u8],
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error> {
        if let Some(tag) = self.results.0.as_ref() {
            let mut msg = Vec::from(msg);
            cipher
                .decrypt_in_place_detached(
                    self.parameters.iv.as_ref().into(),
                    aad,
                    &mut msg,
                    tag.as_ref().into(),
                )
                .map_err(|_| Error::DecryptionFailed)?;
            Ok(msg)
        } else {
            cipher
                .decrypt(
                    self.parameters.iv.as_ref().into(),
                    aes_gcm::aead::Payload { aad, msg },
                )
                .map_err(|_| Error::DecryptionFailed)
        }
        .map(|r| zeroize::Zeroizing::new(r.into()))
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
