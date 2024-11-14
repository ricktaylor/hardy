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

fn update_primary_block<M>(mac: &mut M, args: &OperationArgs, source_data: &[u8])
where
    M: hmac::Mac,
{
    if !args.canonical_primary_block {
        mac.update(&cbor::encode::emit(primary_block::PrimaryBlock::emit(
            args.bundle,
        )));
    } else {
        // This is horrible, but removes a copy
        let data = args
            .bundle
            .blocks
            .get(&0)
            .expect("Missing primary block!")
            .payload(source_data);

        let mut header = cbor::encode::emit(data.len());
        if let Some(m) = header.first_mut() {
            *m |= 2 << 5;
        }
        mac.update(&header);
        mac.update(data);
    }
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

    fn unwrap_key(&self, key: &KeyMaterial) -> Result<Zeroizing<Box<[u8]>>, bpsec::Error> {
        rfc9173::unwrap_key(key, &self.parameters.key).map_field_err("wrapped key")
    }

    pub fn verify(
        &self,
        key: &KeyMaterial,
        args: OperationArgs,
        source_data: &[u8],
    ) -> Result<(), Error> {
        match self.parameters.variant {
            ShaVariant::HMAC_256_256 => self.verify_inner(
                hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.unwrap_key(key)?)
                    .map_field_err("SHA-256 Key")?,
                args,
                source_data,
            ),
            ShaVariant::HMAC_384_384 => self.verify_inner(
                hmac::Hmac::<sha2::Sha384>::new_from_slice(&self.unwrap_key(key)?)
                    .map_field_err("SHA-384 Key")?,
                args,
                source_data,
            ),
            ShaVariant::HMAC_512_512 => self.verify_inner(
                hmac::Hmac::<sha2::Sha512>::new_from_slice(&self.unwrap_key(key)?)
                    .map_field_err("SHA-512 Key")?,
                args,
                source_data,
            ),
            ShaVariant::Unrecognised(_) => Ok(()),
        }
    }

    pub fn verify_inner<M>(
        &self,
        mut mac: M,
        args: OperationArgs,
        source_data: &[u8],
    ) -> Result<(), Error>
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
                update_primary_block(&mut mac, &args, source_data);
            }

            if self.parameters.flags.include_target_header {
                let mut encoder = cbor::encode::Encoder::new();
                encoder.emit(args.target.block_type);
                encoder.emit(*args.target_number);
                encoder.emit(&args.target.flags);
                mac.update(&encoder.build());
            }
        }

        if self.parameters.flags.include_security_header {
            let mut encoder = cbor::encode::Encoder::new();
            encoder.emit(args.source.block_type);
            encoder.emit(*args.source_number);
            encoder.emit(&args.source.flags);
            mac.update(&encoder.build());
        }

        if matches!(args.target.block_type, BlockType::Primary) {
            update_primary_block(&mut mac, &args, source_data);
        } else {
            cbor::decode::parse_value(args.target.payload(source_data), |value, s, tags| {
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
                        mac.update(args.target.payload(source_data));
                    }
                    cbor::decode::Value::Bytes(data) => {
                        // This is horrible, but removes a potentially large data copy
                        let mut header = cbor::encode::emit(data.len());
                        if let Some(m) = header.first_mut() {
                            *m |= 2 << 5;
                        }
                        mac.update(&header);
                        mac.update(data);
                    }
                    _ => unreachable!(),
                }
                Ok::<_, bpsec::Error>(())
            })?;
        }

        if mac.finalize().into_bytes().as_slice() != self.results.0.as_ref() {
            Err(bpsec::Error::IntegrityCheckFailed)
        } else {
            Ok(())
        }
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
