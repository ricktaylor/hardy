use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
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

#[derive(Debug)]
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

#[derive(Debug, Default)]
pub struct Parameters {
    pub variant: ShaVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: ScopeFlags,
}

impl Parameters {
    pub fn from_cbor(
        parameters: &HashMap<u64, Range<usize>>,
        data: &[u8],
    ) -> Result<(Self, bool), bpsec::Error> {
        let mut shortest = true;
        let mut result = Self::default();
        for (id, range) in parameters {
            match id {
                1 => {
                    let (variant, s) = cbor::decode::parse(&data[range.start..range.end])?;
                    result.variant = variant;
                    shortest = shortest && s;
                }
                2 => {
                    let (key, s) = cbor::decode::parse(&data[range.start..range.end])?;
                    result.key = Some(key);
                    shortest = shortest && s;
                }
                3 => {
                    let (flags, s) = cbor::decode::parse(&data[range.start..range.end])?;
                    result.flags = flags;
                    shortest = shortest && s;
                }
                _ => return Err(bpsec::Error::InvalidContextParameter(*id)),
            }
        }
        Ok((result, shortest))
    }
}

#[derive(Debug, Default)]
pub struct Results(Box<[u8]>);

impl Results {
    pub fn from_cbor(
        results: &HashMap<u64, Range<usize>>,
        data: &[u8],
    ) -> Result<(Self, bool), bpsec::Error> {
        let mut shortest = true;
        let mut r = Self::default();
        for (id, range) in results {
            match id {
                1 => {
                    let (value, s) =
                        cbor::decode::parse::<(Box<[u8]>, bool)>(&data[range.start..range.end])?;
                    r.0 = value;
                    shortest = shortest && s;
                }
                _ => return Err(bpsec::Error::InvalidContextResultId(*id)),
            }
        }
        Ok((r, shortest))
    }
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

        let (ValidBundle::Valid(_), true) = cbor::decode::parse(data).unwrap() else {
            panic!("No!");
        };
    }
}
