use super::*;

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
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) -> usize {
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

#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ScopeFlags {
    pub include_primary_block: bool,
    pub include_target_header: bool,
    pub include_security_header: bool,
    pub unrecognised: u64,
}

impl Default for ScopeFlags {
    fn default() -> Self {
        Self {
            include_primary_block: true,
            include_target_header: true,
            include_security_header: true,
            unrecognised: 0,
        }
    }
}

impl cbor::decode::FromCbor for ScopeFlags {
    type Error = cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
            o.map(|(value, shortest, len)| {
                let mut flags = Self {
                    unrecognised: value & !7,
                    ..Default::default()
                };
                for b in 0..=2 {
                    if value & (1 << b) != 0 {
                        match b {
                            0 => flags.include_primary_block = true,
                            1 => flags.include_target_header = true,
                            2 => flags.include_security_header = true,
                            _ => unreachable!(),
                        }
                    }
                }
                (flags, shortest, len)
            })
        })
    }
}

impl cbor::encode::ToCbor for &ScopeFlags {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) -> usize {
        let mut flags = self.unrecognised;
        if self.include_primary_block {
            flags |= 1 << 0;
        }
        if self.include_target_header {
            flags |= 1 << 1;
        }
        if self.include_security_header {
            flags |= 1 << 2;
        }
        encoder.emit(flags)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parameters {
    pub variant: ShaVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: ScopeFlags,
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
                    result.key = Some(cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?);
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
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) -> usize {
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
        encoder.emit_array(Some(mask.count_ones() as usize), |a, _| {
            for b in 1..=3 {
                if mask & (1 << b) != 0 {
                    a.emit_array(Some(2), |a, _| {
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
    pub fn verify(&self, _key: &KeyMaterial, _bundle: &Bundle, _data: &[u8]) -> Result<(), Error> {
        todo!()
    }

    pub fn emit_context(&self, encoder: &mut cbor::encode::Encoder, source: &Eid) -> usize {
        let mut len = encoder.emit(Context::BCB_AES_GCM);
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn rfc9173_appendix_a_1() {
        // Note: I've tweaked the creation timestamp to be valid
        let data = &hex_literal::hex!(
            "9f88070000820282010282028202018202820201820118281a000f4240850b0200
            005856810101018202820201828201078203008181820158403bdc69b3a34a2b5d3a
            8554368bd1e808f606219d2a10a846eae3886ae4ecc83c4ee550fdfb1cc636b904e2
            f1a73e303dcd4b6ccece003e95e8164dcc89a156e185010100005823526561647920
            746f2067656e657261746520612033322d62797465207061796c6f6164ff"
        );

        let ValidBundle::Valid(_) = ValidBundle::parse(data, |_| Ok(None)).unwrap() else {
            panic!("No!");
        };
    }
}
