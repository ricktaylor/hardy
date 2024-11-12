use super::*;
use aes_gcm::KeyInit;
use zeroize::Zeroizing;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AesVariant {
    A128GCM,
    #[default]
    A256GCM,
    Unrecognised(u64),
}

impl cbor::encode::ToCbor for AesVariant {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(match self {
            Self::A128GCM => 1,
            Self::A256GCM => 3,
            Self::Unrecognised(v) => v,
        })
    }
}

impl cbor::decode::FromCbor for AesVariant {
    type Error = cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
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
pub struct Parameters {
    pub iv: Box<[u8]>,
    pub variant: AesVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: bib_hmac_sha2::ScopeFlags,
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
                    variant = Some(cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?);
                }
                3 => {
                    key = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                4 => {
                    flags = Some(cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?);
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

impl cbor::encode::ToCbor for &Parameters {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        let mut mask: u32 = 1 << 1;
        if self.variant != AesVariant::default() {
            mask |= 1 << 2;
        }
        if self.key.is_some() {
            mask |= 1 << 3;
        }
        if self.flags != bib_hmac_sha2::ScopeFlags::default() {
            mask |= 1 << 4;
        }
        encoder.emit_array(Some(mask.count_ones() as usize), |a| {
            for b in 1..=4 {
                if mask & (1 << b) != 0 {
                    a.emit_array(Some(2), |a| {
                        a.emit(b);
                        match b {
                            1 => a.emit(self.iv.as_ref().as_ref()),
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

#[derive(Debug, Default)]
pub struct Results(Box<[u8]>);

impl Results {
    fn from_cbor(
        results: HashMap<u64, Range<usize>>,
        data: &[u8],
    ) -> Result<(Self, bool), bpsec::Error> {
        let mut shortest = true;
        let mut r = Self::default();
        for (id, range) in results {
            match id {
                1 => {
                    r.0 = parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?;
                }
                _ => return Err(bpsec::Error::InvalidContextResultId(id)),
            }
        }
        Ok((r, shortest))
    }
}

impl cbor::encode::ToCbor for &Results {
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
        !matches!(self.parameters.variant, AesVariant::Unrecognised(_))
    }

    fn unwrap_key(&self, _key: &KeyMaterial) -> Zeroizing<Box<[u8]>> {
        // TODO AES-KW
        Zeroizing::new(Box::from(*b"Testing!"))
    }

    pub fn decrypt(
        &self,
        key: &KeyMaterial,
        args: OperationArgs,
        source_data: &[u8],
    ) -> Result<Option<Box<[u8]>>, Error> {
        if matches!(self.parameters.variant, AesVariant::Unrecognised(_)) {
            return Ok(None);
        }

        cbor::decode::parse_value(
            args.target.payload(source_data),
            |value, _, _| match value {
                cbor::decode::Value::ByteStream(data) => match self.parameters.variant {
                    AesVariant::A128GCM => self.decrypt_inplace(
                        aes_gcm::Aes128Gcm::new(self.unwrap_key(key).as_ref().into()),
                        args,
                        source_data,
                        data,
                    ),
                    AesVariant::A256GCM => self.decrypt_inplace(
                        aes_gcm::Aes256Gcm::new(self.unwrap_key(key).as_ref().into()),
                        args,
                        source_data,
                        data,
                    ),
                    AesVariant::Unrecognised(_) => unreachable!(),
                },
                cbor::decode::Value::Bytes(data) => match self.parameters.variant {
                    AesVariant::A128GCM => self.decrypt_copy(
                        aes_gcm::Aes128Gcm::new(self.unwrap_key(key).as_ref().into()),
                        args,
                        source_data,
                        data,
                    ),
                    AesVariant::A256GCM => self.decrypt_copy(
                        aes_gcm::Aes256Gcm::new(self.unwrap_key(key).as_ref().into()),
                        args,
                        source_data,
                        data,
                    ),
                    AesVariant::Unrecognised(_) => unreachable!(),
                },
                _ => unreachable!(),
            },
        )
        .map(|v| v.0)
    }

    fn decrypt_copy<M>(
        &self,
        cipher: M,
        args: OperationArgs,
        source_data: &[u8],
        data: &[u8],
    ) -> Result<Option<Box<[u8]>>, bpsec::Error>
    where
        M: aes_gcm::aead::Aead,
    {
        cipher
            .decrypt(
                self.parameters.iv.as_ref().into(),
                aes_gcm::aead::Payload {
                    msg: data,
                    aad: &self.gen_aad(args, source_data),
                },
            )
            .map(|d| Some(d.into()))
            .map_err(|_| bpsec::Error::DecryptionFailed)
    }

    fn decrypt_inplace<M>(
        &self,
        cipher: M,
        args: OperationArgs,
        source_data: &[u8],
        data: &[&[u8]],
    ) -> Result<Option<Box<[u8]>>, bpsec::Error>
    where
        M: aes_gcm::aead::AeadInPlace,
    {
        // Concatenate all the bytes
        let mut data: Vec<u8> = data.iter().fold(Vec::new(), |mut data, d| {
            data.extend(*d);
            data
        });

        // Then decrypt in-place, this results in a single data copy
        cipher
            .decrypt_in_place(
                self.parameters.iv.as_ref().into(),
                &self.gen_aad(args, source_data),
                &mut data,
            )
            .map(|_| Some(data.into()))
            .map_err(|_| bpsec::Error::DecryptionFailed)
    }

    fn gen_aad(&self, args: OperationArgs, source_data: &[u8]) -> Vec<u8> {
        let mut encoder = cbor::encode::Encoder::new();
        encoder.emit(&bib_hmac_sha2::ScopeFlags {
            include_primary_block: self.parameters.flags.include_primary_block,
            include_target_header: self.parameters.flags.include_target_header,
            include_security_header: self.parameters.flags.include_security_header,
            ..Default::default()
        });

        if self.parameters.flags.include_primary_block {
            if !args.canonical_primary_block {
                encoder.emit_raw(primary_block::PrimaryBlock::emit(args.bundle));
            } else {
                encoder.emit_raw_slice(
                    args.bundle
                        .blocks
                        .get(&0)
                        .expect("Missing primary block!")
                        .payload(source_data),
                );
            }
        }

        if self.parameters.flags.include_target_header {
            encoder.emit(args.target.block_type);
            encoder.emit(*args.target_number);
            encoder.emit(&args.target.flags);
        }

        if self.parameters.flags.include_security_header {
            encoder.emit(args.source.block_type);
            encoder.emit(*args.source_number);
            encoder.emit(&args.source.flags);
        }

        encoder.build()
    }

    pub fn emit_context(&self, encoder: &mut cbor::encode::Encoder, source: &Eid) {
        encoder.emit(Context::BIB_HMAC_SHA2);
        encoder.emit(1);
        encoder.emit(source);
        encoder.emit(self.parameters.as_ref());
    }

    pub fn emit_result(self, array: &mut cbor::encode::Array) {
        array.emit(&self.results);
    }
}

pub fn parse(
    asb: parse::AbstractSyntaxBlock,
    data: &[u8],
    shortest: &mut bool,
) -> Result<(Eid, HashMap<u64, bcb::Operation>), Error> {
    let parameters = Rc::from(
        Parameters::from_cbor(asb.parameters, data)
            .map(|(p, s)| {
                *shortest = *shortest && s;
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
                        *shortest = *shortest && s;
                        v
                    })
                    .map_field_err("RFC9173 AES-GCM results")?,
            }),
        );
    }
    Ok((asb.source, operations))
}
