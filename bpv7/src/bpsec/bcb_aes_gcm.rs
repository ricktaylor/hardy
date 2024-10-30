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

impl cbor::encode::ToCbor for AesVariant {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) -> usize {
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

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parameters {
    pub iv: Option<Box<[u8]>>,
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
        let mut result = Self::default();
        for (id, range) in parameters {
            match id {
                1 => {
                    result.iv = Some(cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?);
                }
                2 => {
                    result.variant =
                        cbor::decode::parse(&data[range.start..range.end]).map(|(v, s)| {
                            shortest = shortest && s;
                            v
                        })?;
                }
                3 => {
                    result.key = Some(cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?);
                }
                4 => {
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
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) -> usize {
        let mut mask: u32 = 0;
        if self.iv.is_some() {
            mask |= 1 << 1;
        }
        if self.variant != AesVariant::default() {
            mask |= 1 << 2;
        }
        if self.key.is_some() {
            mask |= 1 << 3;
        }
        if self.flags != bib_hmac_sha2::ScopeFlags::default() {
            mask |= 1 << 4;
        }
        encoder.emit_array(Some(mask.count_ones() as usize), |a, _| {
            for b in 1..=4 {
                if mask & (1 << b) != 0 {
                    a.emit_array(Some(2), |a, _| {
                        a.emit(b);
                        match b {
                            1 => a.emit(self.iv.as_ref().unwrap().as_ref()),
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
                    r.0 = cbor::decode::parse::<(Box<[u8]>, bool)>(&data[range.start..range.end])
                        .map(|(v, s)| {
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
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) -> usize {
        encoder.emit_array(Some(1), |a, _| {
            a.emit_array(Some(2), |a, _| {
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
    pub fn decrypt(
        &self,
        _key: &KeyMaterial,
        _bundle: &Bundle,
        _data: &[u8],
    ) -> Result<Box<[u8]>, Error> {
        todo!()
    }

    pub fn emit_context(&self, encoder: &mut cbor::encode::Encoder, source: &Eid) -> usize {
        let mut len = encoder.emit(Context::BIB_HMAC_SHA2);
        if self.parameters.as_ref() == &Parameters::default() {
            len += encoder.emit(0);
            len + encoder.emit(source)
        } else {
            len += encoder.emit(1);
            len += encoder.emit(source);
            len + encoder.emit(self.parameters.as_ref())
        }
    }

    pub fn emit_result(&self, array: &mut cbor::encode::Array) {
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
