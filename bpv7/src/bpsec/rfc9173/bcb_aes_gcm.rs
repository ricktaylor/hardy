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
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(match self {
            Self::A128GCM => 1,
            Self::A256GCM => 3,
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
    ) -> Result<(Self, bool), bpsec::Error> {
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
                _ => return Err(bpsec::Error::InvalidContextParameter(id)),
            }
        }
        let Some(iv) = iv else {
            return Err(bpsec::Error::MissingContextParameter(1));
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

impl hardy_cbor::encode::ToCbor for &Parameters {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
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
                        a.emit(b);
                        match b {
                            1 => a.emit(self.iv.as_ref()),
                            2 => a.emit(self.variant),
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
struct Results(Box<[u8]>);

impl Results {
    fn from_cbor(
        results: HashMap<u64, Range<usize>>,
        data: &[u8],
    ) -> Result<(Self, bool), bpsec::Error> {
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
                _ => return Err(bpsec::Error::InvalidContextResult(id)),
            }
        }

        Ok((
            Self(r.ok_or(bpsec::Error::InvalidContextResult(1))?),
            shortest,
        ))
    }
}

impl hardy_cbor::encode::ToCbor for &Results {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit_array(Some(1), |a| {
            a.emit_array(Some(2), |a| {
                a.emit(1);
                a.emit(self.0.as_ref());
            });
        })
    }
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

    pub fn encrypt<'a>(
        &mut self,
        key_f: impl Fn(&eid::Eid, bpsec::key::Operation) -> Result<Option<&'a bpsec::Key>, bpsec::Error>,
        args: bcb::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<Box<[u8]>, Error> {
        if let Some(cek) = &self.parameters.key {
            let cek = unwrap_key(args.bpsec_source, key_f, cek)?;
            let (data, aad) = self.build_data(&args, payload_data)?;

            match self.parameters.variant {
                AesVariant::A128GCM => self.encrypt_inner(
                    &mut aes_gcm::Aes128Gcm::new_from_slice(&cek).map_field_err("AES-128 key")?,
                    aad,
                    data,
                ),
                AesVariant::A256GCM => self.encrypt_inner(
                    &mut aes_gcm::Aes256Gcm::new_from_slice(&cek).map_field_err("AES-256 key")?,
                    aad,
                    data,
                ),
                AesVariant::Unrecognised(_) => unreachable!(),
            }
        } else {
            let Some(jwk) = key_f(args.bpsec_source, key::Operation::Encrypt)? else {
                return Err(Error::NoKey(args.bpsec_source.clone()));
            };

            if let Some(algorithm) = &jwk.key_algorithm {
                if !matches!(algorithm, key::KeyAlgorithm::Direct) {
                    return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
                }
            };
            let Some(algorithm) = &jwk.enc_algorithm else {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            };

            if self.parameters.variant
                != match algorithm {
                    key::EncAlgorithm::A128GCM => AesVariant::A128GCM,
                    key::EncAlgorithm::A256GCM => AesVariant::A256GCM,
                    _ => AesVariant::Unrecognised(0),
                }
            {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            }

            if let Some(ops) = &jwk.operations {
                if ops.iter().any(|v| matches!(v, key::Operation::Encrypt)) {
                    return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
                }
            }

            let key::Type::OctetSequence { key: cek } = &jwk.key_type else {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            };

            let (data, aad) = self.build_data(&args, payload_data)?;

            match self.parameters.variant {
                AesVariant::A128GCM => self.encrypt_inner(
                    &mut aes_gcm::Aes128Gcm::new_from_slice(cek).map_field_err("AES-128 key")?,
                    aad,
                    data,
                ),
                AesVariant::A256GCM => self.encrypt_inner(
                    &mut aes_gcm::Aes256Gcm::new_from_slice(cek).map_field_err("AES-256 key")?,
                    aad,
                    data,
                ),
                AesVariant::Unrecognised(_) => unreachable!(),
            }
        }
    }

    pub fn decrypt<'a>(
        &self,
        key_f: impl Fn(&eid::Eid, bpsec::key::Operation) -> Result<Option<&'a bpsec::Key>, bpsec::Error>,
        args: bcb::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<bcb::OperationResult, Error> {
        if let Some(cek) = &self.parameters.key {
            let cek = match unwrap_key(args.bpsec_source, key_f, cek) {
                Ok(cek) => cek,
                Err(Error::NoKey(..)) => {
                    return Ok(bcb::OperationResult {
                        plaintext: None,
                        protects_primary_block: self.parameters.flags.include_primary_block,
                        can_encrypt: false,
                    });
                }
                Err(e) => return Err(e),
            };
            let (data, aad) = self.build_data(&args, payload_data)?;

            match self.parameters.variant {
                AesVariant::A128GCM => self.decrypt_inner(
                    &mut aes_gcm::Aes128Gcm::new_from_slice(&cek).map_field_err("AES-128 key")?,
                    aad,
                    data,
                ),
                AesVariant::A256GCM => self.decrypt_inner(
                    &mut aes_gcm::Aes256Gcm::new_from_slice(&cek).map_field_err("AES-256 key")?,
                    aad,
                    data,
                ),
                AesVariant::Unrecognised(_) => Ok(bcb::OperationResult {
                    plaintext: None,
                    protects_primary_block: self.parameters.flags.include_primary_block,
                    can_encrypt: false,
                }),
            }
        } else {
            let Some(jwk) = key_f(args.bpsec_source, key::Operation::Decrypt)? else {
                return Ok(bcb::OperationResult {
                    plaintext: None,
                    protects_primary_block: self.parameters.flags.include_primary_block,
                    can_encrypt: false,
                });
            };

            if let Some(algorithm) = &jwk.key_algorithm {
                if !matches!(algorithm, key::KeyAlgorithm::Direct) {
                    return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
                }
            };
            let Some(algorithm) = &jwk.enc_algorithm else {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            };

            if self.parameters.variant
                != match algorithm {
                    key::EncAlgorithm::A128GCM => AesVariant::A128GCM,
                    key::EncAlgorithm::A256GCM => AesVariant::A256GCM,
                    _ => AesVariant::Unrecognised(0),
                }
            {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            }

            let key::Type::OctetSequence { key: cek } = &jwk.key_type else {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            };

            let (data, aad) = self.build_data(&args, payload_data)?;

            match self.parameters.variant {
                AesVariant::A128GCM => self.decrypt_inner(
                    &mut aes_gcm::Aes128Gcm::new_from_slice(cek).map_field_err("AES-128 key")?,
                    aad,
                    data,
                ),
                AesVariant::A256GCM => self.decrypt_inner(
                    &mut aes_gcm::Aes256Gcm::new_from_slice(cek).map_field_err("AES-256 key")?,
                    aad,
                    data,
                ),
                AesVariant::Unrecognised(_) => Ok(bcb::OperationResult {
                    plaintext: None,
                    protects_primary_block: self.parameters.flags.include_primary_block,
                    can_encrypt: false,
                }),
            }
        }
    }

    fn build_data(
        &self,
        args: &bcb::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<(Vec<u8>, Vec<u8>), Error> {
        let mut encoder = hardy_cbor::encode::Encoder::new();
        encoder.emit(&ScopeFlags {
            include_primary_block: self.parameters.flags.include_primary_block,
            include_target_header: self.parameters.flags.include_target_header,
            include_security_header: self.parameters.flags.include_security_header,
            ..Default::default()
        });

        if self.parameters.flags.include_primary_block {
            encoder.emit_raw_slice(args.primary_block);
        }

        if self.parameters.flags.include_target_header {
            encoder.emit(args.target.block_type);
            encoder.emit(args.target_number);
            encoder.emit(&args.target.flags);
        }

        if self.parameters.flags.include_security_header {
            encoder.emit(args.source.block_type);
            encoder.emit(args.source_number);
            encoder.emit(&args.source.flags);
        }
        let aad = encoder.build();

        let mut data = if let Some(payload_data) = payload_data {
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

        // Append authentication tag
        data.extend_from_slice(&self.results.0);

        Ok((data, aad))
    }

    fn decrypt_inner(
        &self,
        cipher: &mut impl aes_gcm::aead::AeadInPlace,
        aad: Vec<u8>,
        mut data: Vec<u8>,
    ) -> Result<bcb::OperationResult, Error> {
        // Decrypt in-place, this results in a single data copy
        cipher
            .decrypt_in_place(self.parameters.iv.as_ref().into(), &aad, &mut data)
            .map(|_| bcb::OperationResult {
                plaintext: Some(zeroize::Zeroizing::new(data.into())),
                protects_primary_block: self.parameters.flags.include_primary_block,
                can_encrypt: true,
            })
            .map_err(|_| bpsec::Error::DecryptionFailed)
    }

    fn encrypt_inner(
        &self,
        cipher: &mut impl aes_gcm::aead::AeadInPlace,
        aad: Vec<u8>,
        mut data: Vec<u8>,
    ) -> Result<Box<[u8]>, Error> {
        // Encrypt in-place, this results in a single data copy
        cipher
            .encrypt_in_place(self.parameters.iv.as_ref().into(), &aad, &mut data)
            .map(|_| data.into())
            .map_err(|_| bpsec::Error::EncryptionFailed)
    }

    pub fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
        encoder.emit(Context::BIB_HMAC_SHA2);
        encoder.emit(1);
        encoder.emit(source);
        encoder.emit(self.parameters.as_ref());
    }

    pub fn emit_result(self, array: &mut hardy_cbor::encode::Array) {
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
