use super::*;
use aes_gcm::aes::cipher::BlockSizeUser;
use alloc::rc::Rc;
use hmac::{
    Mac,
    digest::{
        HashMarker, block_buffer,
        consts::U256,
        core_api::{BufferKindUser, CoreProxy, FixedOutputCore, UpdateCore},
        typenum,
    },
};

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ShaVariant {
    HMAC_256_256,
    #[default]
    HMAC_384_384,
    HMAC_512_512,
    Unrecognised(u64),
}

impl hardy_cbor::encode::ToCbor for ShaVariant {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(match self {
            Self::HMAC_256_256 => 5,
            Self::HMAC_384_384 => 6,
            Self::HMAC_512_512 => 7,
            Self::Unrecognised(v) => v,
        })
    }
}

impl hardy_cbor::decode::FromCbor for ShaVariant {
    type Error = hardy_cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
            o.map(|(value, shortest, len)| {
                (
                    match value {
                        5 => Self::HMAC_256_256,
                        6 => Self::HMAC_384_384,
                        7 => Self::HMAC_512_512,
                        v => Self::Unrecognised(v),
                    },
                    shortest,
                    len,
                )
            })
        })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Parameters {
    variant: ShaVariant,
    key: Option<Box<[u8]>>,
    flags: ScopeFlags,
}

impl Parameters {
    fn from_cbor(
        parameters: HashMap<u64, Range<usize>>,
        data: &[u8],
    ) -> Result<(Self, bool), bpsec::Error> {
        let mut shortest = true;
        let mut result = Self::default();
        for (id, range) in parameters {
            match id {
                1 => {
                    result.variant = hardy_cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?;
                }
                2 => {
                    result.key = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                3 => {
                    result.flags = hardy_cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?;
                }
                _ => return Err(bpsec::Error::InvalidContextParameter(id)),
            }
        }
        Ok((result, shortest))
    }
}

impl hardy_cbor::encode::ToCbor for &Parameters {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        let mut mask: u32 = 0;
        if self.variant != ShaVariant::default() {
            mask |= 1 << 1;
        }
        if self.key.is_some() {
            mask |= 1 << 2;
        }
        if self.flags != ScopeFlags::default() {
            mask |= 1 << 3;
        }
        encoder.emit_array(Some(mask.count_ones() as usize), |a| {
            for b in 1..=3 {
                if mask & (1 << b) != 0 {
                    a.emit_array(Some(2), |a| {
                        a.emit(b);
                        match b {
                            1 => a.emit(self.variant),
                            2 => a.emit(self.key.as_ref().unwrap().as_ref()),
                            3 => a.emit(&self.flags),
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

fn emit_data(mac: &mut impl hmac::Mac, data: &[u8]) {
    let mut header = hardy_cbor::encode::emit(data.len());
    if let Some(m) = header.first_mut() {
        *m |= 2 << 5;
    }
    mac.update(&header);
    mac.update(data);
}

#[derive(Debug)]
pub struct Operation {
    parameters: Rc<Parameters>,
    results: Results,
}

impl Operation {
    pub fn is_unsupported(&self) -> bool {
        matches!(self.parameters.variant, ShaVariant::Unrecognised(_))
    }

    pub fn sign<'a>(
        &mut self,
        key_f: impl Fn(&eid::Eid, bpsec::key::Operation) -> Result<Option<&'a bpsec::Key>, bpsec::Error>,
        args: bib::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<(), Error> {
        self.results.0 = if let Some(cek) = &self.parameters.key {
            let cek = unwrap_key(args.bpsec_source, key_f, cek)?;
            match self.parameters.variant {
                ShaVariant::HMAC_256_256 => self
                    .calculate_hmac::<sha2::Sha256>(&cek, &args, payload_data)?
                    .as_slice()
                    .into(),
                ShaVariant::HMAC_384_384 => self
                    .calculate_hmac::<sha2::Sha384>(&cek, &args, payload_data)?
                    .as_slice()
                    .into(),
                ShaVariant::HMAC_512_512 => self
                    .calculate_hmac::<sha2::Sha512>(&cek, &args, payload_data)?
                    .as_slice()
                    .into(),
                ShaVariant::Unrecognised(_) => unreachable!(),
            }
        } else {
            let Some(jwk) = key_f(args.bpsec_source, key::Operation::Sign)? else {
                return Err(Error::NoKey(args.bpsec_source.clone()));
            };

            let Some(algorithm) = &jwk.key_algorithm else {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            };

            if self.parameters.variant
                != match algorithm {
                    key::KeyAlgorithm::HS256 => ShaVariant::HMAC_256_256,
                    key::KeyAlgorithm::HS384 => ShaVariant::HMAC_384_384,
                    key::KeyAlgorithm::HS512 => ShaVariant::HMAC_512_512,
                    _ => ShaVariant::Unrecognised(0),
                }
            {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            }

            let key::Type::OctetSequence { key: cek } = &jwk.key_type else {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            };

            match self.parameters.variant {
                ShaVariant::HMAC_256_256 => self
                    .calculate_hmac::<sha2::Sha256>(cek, &args, payload_data)?
                    .as_slice()
                    .into(),
                ShaVariant::HMAC_384_384 => self
                    .calculate_hmac::<sha2::Sha384>(cek, &args, payload_data)?
                    .as_slice()
                    .into(),
                ShaVariant::HMAC_512_512 => self
                    .calculate_hmac::<sha2::Sha512>(cek, &args, payload_data)?
                    .as_slice()
                    .into(),
                ShaVariant::Unrecognised(_) => unreachable!(),
            }
        };
        Ok(())
    }

    pub fn verify<'a>(
        &self,
        key_f: impl Fn(&eid::Eid, bpsec::key::Operation) -> Result<Option<&'a bpsec::Key>, bpsec::Error>,
        args: bib::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<bib::OperationResult, Error> {
        let can_sign = if let Some(cek) = &self.parameters.key {
            let cek = match unwrap_key(args.bpsec_source, key_f, cek) {
                Ok(cek) => cek,
                Err(Error::NoKey(..)) => {
                    return Ok(bib::OperationResult {
                        protects_primary_block: args.target_number == 0
                            || self.parameters.flags.include_primary_block,
                        can_sign: false,
                    });
                }
                Err(e) => return Err(e),
            };
            match self.parameters.variant {
                ShaVariant::HMAC_256_256 => {
                    if self
                        .calculate_hmac::<sha2::Sha256>(&cek, &args, payload_data)?
                        .as_slice()
                        != self.results.0.as_ref()
                    {
                        return Err(bpsec::Error::IntegrityCheckFailed);
                    }
                    true
                }
                ShaVariant::HMAC_384_384 => {
                    if self
                        .calculate_hmac::<sha2::Sha384>(&cek, &args, payload_data)?
                        .as_slice()
                        != self.results.0.as_ref()
                    {
                        return Err(bpsec::Error::IntegrityCheckFailed);
                    }
                    true
                }
                ShaVariant::HMAC_512_512 => {
                    if self
                        .calculate_hmac::<sha2::Sha512>(&cek, &args, payload_data)?
                        .as_slice()
                        != self.results.0.as_ref()
                    {
                        return Err(bpsec::Error::IntegrityCheckFailed);
                    }
                    true
                }
                ShaVariant::Unrecognised(_) => false,
            }
        } else {
            let Some(jwk) = key_f(args.bpsec_source, key::Operation::Verify)? else {
                return Ok(bib::OperationResult {
                    protects_primary_block: args.target_number == 0
                        || self.parameters.flags.include_primary_block,
                    can_sign: false,
                });
            };

            let Some(algorithm) = &jwk.key_algorithm else {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            };

            if self.parameters.variant
                != match algorithm {
                    key::KeyAlgorithm::HS256 => ShaVariant::HMAC_256_256,
                    key::KeyAlgorithm::HS384 => ShaVariant::HMAC_384_384,
                    key::KeyAlgorithm::HS512 => ShaVariant::HMAC_512_512,
                    _ => ShaVariant::Unrecognised(0),
                }
            {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            }

            let key::Type::OctetSequence { key: cek } = &jwk.key_type else {
                return Err(bpsec::Error::NoKey(args.bpsec_source.clone()));
            };

            match self.parameters.variant {
                ShaVariant::HMAC_256_256 => {
                    if self
                        .calculate_hmac::<sha2::Sha256>(cek, &args, payload_data)?
                        .as_slice()
                        != self.results.0.as_ref()
                    {
                        return Err(bpsec::Error::IntegrityCheckFailed);
                    }
                    true
                }
                ShaVariant::HMAC_384_384 => {
                    if self
                        .calculate_hmac::<sha2::Sha384>(cek, &args, payload_data)?
                        .as_slice()
                        != self.results.0.as_ref()
                    {
                        return Err(bpsec::Error::IntegrityCheckFailed);
                    }
                    true
                }
                ShaVariant::HMAC_512_512 => {
                    if self
                        .calculate_hmac::<sha2::Sha512>(cek, &args, payload_data)?
                        .as_slice()
                        != self.results.0.as_ref()
                    {
                        return Err(bpsec::Error::IntegrityCheckFailed);
                    }
                    true
                }
                ShaVariant::Unrecognised(_) => false,
            }
        };

        Ok(bib::OperationResult {
            protects_primary_block: args.target_number == 0
                || self.parameters.flags.include_primary_block,
            can_sign,
        })
    }

    fn calculate_hmac<A>(
        &self,
        key: &[u8],
        args: &bib::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<hmac::digest::Output<hmac::Hmac<A>>, Error>
    where
        A: CoreProxy,
        <A as CoreProxy>::Core: HashMarker
            + UpdateCore
            + FixedOutputCore
            + BufferKindUser<BufferKind = block_buffer::Eager>
            + Default
            + Clone,
        <<A as CoreProxy>::Core as BlockSizeUser>::BlockSize: typenum::IsLess<U256>,
        typenum::Le<<<A as CoreProxy>::Core as BlockSizeUser>::BlockSize, U256>: typenum::NonZero,
    {
        let mut mac = hmac::Hmac::<A>::new_from_slice(key).map_field_err("Invalid key length")?;

        // Build IPT
        mac.update(&hardy_cbor::encode::emit(&ScopeFlags {
            include_primary_block: self.parameters.flags.include_primary_block,
            include_target_header: self.parameters.flags.include_target_header,
            include_security_header: self.parameters.flags.include_security_header,
            ..Default::default()
        }));

        if !matches!(args.target.block_type, block::Type::Primary) {
            if self.parameters.flags.include_primary_block {
                mac.update(args.primary_block);
            }

            if self.parameters.flags.include_target_header {
                let mut encoder = hardy_cbor::encode::Encoder::new();
                encoder.emit(args.target.block_type);
                encoder.emit(args.target_number);
                encoder.emit(&args.target.flags);
                mac.update(&encoder.build());
            }
        }

        if self.parameters.flags.include_security_header {
            let mut encoder = hardy_cbor::encode::Encoder::new();
            encoder.emit(args.source.block_type);
            encoder.emit(args.source_number);
            encoder.emit(&args.source.flags);
            mac.update(&encoder.build());
        }

        if matches!(args.target.block_type, block::Type::Primary) {
            emit_data(&mut mac, args.primary_block);
        } else if let Some(payload_data) = payload_data {
            emit_data(&mut mac, payload_data);
        } else {
            hardy_cbor::decode::parse_value(args.target_payload, |value, s, tags| {
                match value {
                    hardy_cbor::decode::Value::ByteStream(data) => {
                        // This is horrible, but removes a potentially large data copy
                        let len = data.iter().try_fold(0u64, |len, d| {
                            len.checked_add(d.len() as u64)
                                .ok_or(bpsec::Error::InvalidBIBTarget)
                        })?;
                        let mut header = hardy_cbor::encode::emit(len);
                        if let Some(m) = header.first_mut() {
                            *m |= 2 << 5;
                        }
                        mac.update(&header);
                        for d in data {
                            mac.update(d);
                        }
                    }
                    hardy_cbor::decode::Value::Bytes(_) if s && tags.is_empty() => {
                        mac.update(args.target_payload);
                    }
                    hardy_cbor::decode::Value::Bytes(data) => {
                        emit_data(&mut mac, data);
                    }
                    _ => unreachable!(),
                }
                Ok::<_, bpsec::Error>(())
            })?;
        }

        Ok(mac.finalize().into_bytes())
    }

    pub fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
        encoder.emit(Context::BIB_HMAC_SHA2);
        if self.parameters.as_ref() == &Parameters::default() {
            encoder.emit(0);
            encoder.emit(source);
        } else {
            encoder.emit(1);
            encoder.emit(source);
            encoder.emit(self.parameters.as_ref());
        }
    }

    pub fn emit_result(self, array: &mut hardy_cbor::encode::Array) {
        array.emit(&self.results);
    }
}

pub fn parse(
    asb: parse::AbstractSyntaxBlock,
    data: &[u8],
) -> Result<(eid::Eid, HashMap<u64, bib::Operation>, bool), Error> {
    let mut shortest = false;
    let parameters = Rc::from(
        Parameters::from_cbor(asb.parameters, data)
            .map(|(p, s)| {
                shortest = s;
                p
            })
            .map_field_err("RFC9173 HMAC-SHA2 parameters")?,
    );

    // Unpack results
    let mut operations = HashMap::new();
    for (target, results) in asb.results {
        operations.insert(
            target,
            bib::Operation::HMAC_SHA2(Operation {
                parameters: parameters.clone(),
                results: Results::from_cbor(results, data)
                    .map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })
                    .map_field_err("RFC9173 HMAC-SHA2 results")?,
            }),
        );
    }
    Ok((asb.source, operations, shortest))
}
