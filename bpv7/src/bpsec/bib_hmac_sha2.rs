use super::*;
use hmac::Mac;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShaVariant {
    HMAC_256_256,
    #[default]
    HMAC_384_384,
    HMAC_512_512,
    Unrecognised(u64),
}

impl cbor::encode::ToCbor for ShaVariant {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(match self {
            Self::HMAC_256_256 => 5,
            Self::HMAC_384_384 => 6,
            Self::HMAC_512_512 => 7,
            Self::Unrecognised(v) => v,
        })
    }
}

impl cbor::decode::FromCbor for ShaVariant {
    type Error = cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
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
pub struct Parameters {
    pub variant: ShaVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: rfc9173::ScopeFlags,
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
                    result.variant =
                        cbor::decode::parse(&data[range.start..range.end]).map(|(v, s)| {
                            shortest = shortest && s;
                            v
                        })?;
                }
                2 => {
                    result.key = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                3 => {
                    result.flags =
                        cbor::decode::parse(&data[range.start..range.end]).map(|(v, s)| {
                            shortest = shortest && s;
                            v
                        })?;
                }
                _ => return Err(bpsec::Error::InvalidContextParameter(id)),
            }
        }
        Ok((result, shortest))
    }
}

impl cbor::encode::ToCbor for &Parameters {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        let mut mask: u32 = 0;
        if self.variant != ShaVariant::default() {
            mask |= 1 << 1;
        }
        if self.key.is_some() {
            mask |= 1 << 2;
        }
        if self.flags != rfc9173::ScopeFlags::default() {
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
pub struct Results(Box<[u8]>);

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

fn emit_data(mac: &mut impl hmac::Mac, data: &[u8]) {
    let mut header = cbor::encode::emit(data.len());
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

    pub fn sign(
        &mut self,
        key: Option<&KeyMaterial>,
        args: bib::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<(), Error> {
        let Some(key) = key else {
            return Err(Error::NoKey(args.bpsec_source.clone()));
        };
        let key = rfc9173::unwrap_key(args.bpsec_source, key, &self.parameters.key)?;

        self.results.0 = match self.parameters.variant {
            ShaVariant::HMAC_256_256 => self
                .calculate_hmac(
                    hmac::Hmac::<sha2::Sha256>::new_from_slice(&key)
                        .map_field_err("SHA-256 key")?,
                    &args,
                    payload_data,
                )?
                .into_bytes()
                .as_slice()
                .into(),
            ShaVariant::HMAC_384_384 => self
                .calculate_hmac(
                    hmac::Hmac::<sha2::Sha384>::new_from_slice(&key)
                        .map_field_err("SHA-384 key")?,
                    &args,
                    payload_data,
                )?
                .into_bytes()
                .as_slice()
                .into(),
            ShaVariant::HMAC_512_512 => self
                .calculate_hmac(
                    hmac::Hmac::<sha2::Sha512>::new_from_slice(&key)
                        .map_field_err("SHA-512 key")?,
                    &args,
                    payload_data,
                )?
                .into_bytes()
                .as_slice()
                .into(),
            ShaVariant::Unrecognised(_) => unreachable!(),
        };
        Ok(())
    }

    pub fn verify(
        &self,
        key: Option<&KeyMaterial>,
        args: bib::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<bib::OperationResult, Error> {
        let Some(key) = key else {
            return Ok(bib::OperationResult {
                protects_primary_block: args.target_number == 0
                    || self.parameters.flags.include_primary_block,
                can_sign: false,
            });
        };
        let key = rfc9173::unwrap_key(args.bpsec_source, key, &self.parameters.key)?;

        let can_sign = match self.parameters.variant {
            ShaVariant::HMAC_256_256 => {
                if self
                    .calculate_hmac(
                        hmac::Hmac::<sha2::Sha256>::new_from_slice(&key)
                            .map_field_err("SHA-256 key")?,
                        &args,
                        payload_data,
                    )?
                    .into_bytes()
                    .as_slice()
                    != self.results.0.as_ref()
                {
                    return Err(bpsec::Error::IntegrityCheckFailed);
                }
                true
            }
            ShaVariant::HMAC_384_384 => {
                if self
                    .calculate_hmac(
                        hmac::Hmac::<sha2::Sha384>::new_from_slice(&key)
                            .map_field_err("SHA-384 key")?,
                        &args,
                        payload_data,
                    )?
                    .into_bytes()
                    .as_slice()
                    != self.results.0.as_ref()
                {
                    return Err(bpsec::Error::IntegrityCheckFailed);
                }
                true
            }
            ShaVariant::HMAC_512_512 => {
                if self
                    .calculate_hmac(
                        hmac::Hmac::<sha2::Sha512>::new_from_slice(&key)
                            .map_field_err("SHA-512 key")?,
                        &args,
                        payload_data,
                    )?
                    .into_bytes()
                    .as_slice()
                    != self.results.0.as_ref()
                {
                    return Err(bpsec::Error::IntegrityCheckFailed);
                }
                true
            }
            ShaVariant::Unrecognised(_) => false,
        };

        Ok(bib::OperationResult {
            protects_primary_block: args.target_number == 0
                || self.parameters.flags.include_primary_block,
            can_sign,
        })
    }

    pub fn calculate_hmac<M>(
        &self,
        mut mac: M,
        args: &bib::OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<hmac::digest::CtOutput<M>, Error>
    where
        M: hmac::Mac,
    {
        // Build IPT
        mac.update(&cbor::encode::emit(&rfc9173::ScopeFlags {
            include_primary_block: self.parameters.flags.include_primary_block,
            include_target_header: self.parameters.flags.include_target_header,
            include_security_header: self.parameters.flags.include_security_header,
            ..Default::default()
        }));

        if !matches!(args.target.block_type, BlockType::Primary) {
            if self.parameters.flags.include_primary_block {
                mac.update(args.primary_block);
            }

            if self.parameters.flags.include_target_header {
                let mut encoder = cbor::encode::Encoder::new();
                encoder.emit(args.target.block_type);
                encoder.emit(args.target_number);
                encoder.emit(&args.target.flags);
                mac.update(&encoder.build());
            }
        }

        if self.parameters.flags.include_security_header {
            let mut encoder = cbor::encode::Encoder::new();
            encoder.emit(args.source.block_type);
            encoder.emit(args.source_number);
            encoder.emit(&args.source.flags);
            mac.update(&encoder.build());
        }

        if matches!(args.target.block_type, BlockType::Primary) {
            emit_data(&mut mac, args.primary_block);
        } else if let Some(payload_data) = payload_data {
            emit_data(&mut mac, payload_data);
        } else {
            cbor::decode::parse_value(args.target_payload, |value, s, tags| {
                match value {
                    cbor::decode::Value::ByteStream(data) => {
                        // This is horrible, but removes a potentially large data copy
                        let len = data.iter().try_fold(0u64, |len, d| {
                            len.checked_add(d.len() as u64)
                                .ok_or(bpsec::Error::InvalidBIBTarget)
                        })?;
                        let mut header = cbor::encode::emit(len);
                        if let Some(m) = header.first_mut() {
                            *m |= 2 << 5;
                        }
                        mac.update(&header);
                        for d in data {
                            mac.update(d);
                        }
                    }
                    cbor::decode::Value::Bytes(_) if s && tags.is_empty() => {
                        mac.update(args.target_payload);
                    }
                    cbor::decode::Value::Bytes(data) => {
                        emit_data(&mut mac, data);
                    }
                    _ => unreachable!(),
                }
                Ok::<_, bpsec::Error>(())
            })?;
        }

        Ok(mac.finalize())
    }

    pub fn emit_context(&self, encoder: &mut cbor::encode::Encoder, source: &Eid) {
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

    pub fn emit_result(self, array: &mut cbor::encode::Array) {
        array.emit(&self.results);
    }
}

pub fn parse(
    asb: parse::AbstractSyntaxBlock,
    data: &[u8],
    shortest: &mut bool,
) -> Result<(Eid, HashMap<u64, bib::Operation>), Error> {
    let parameters = Rc::from(
        Parameters::from_cbor(asb.parameters, data)
            .map(|(p, s)| {
                *shortest = *shortest && s;
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
                        *shortest = *shortest && s;
                        v
                    })
                    .map_field_err("RFC9173 HMAC-SHA2 results")?,
            }),
        );
    }
    Ok((asb.source, operations))
}
