use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
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

#[derive(Debug, Default)]
pub struct Parameters {
    pub iv: Option<Box<[u8]>>,
    pub variant: AesVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: bib_hmac_sha2::ScopeFlags,
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
                    let (iv, s) = cbor::decode::parse(&data[range.start..range.end])?;
                    result.iv = Some(iv);
                    shortest = shortest && s;
                }
                2 => {
                    let (variant, s) = cbor::decode::parse(&data[range.start..range.end])?;
                    result.variant = variant;
                    shortest = shortest && s;
                }
                3 => {
                    let (key, s) = cbor::decode::parse(&data[range.start..range.end])?;
                    result.key = Some(key);
                    shortest = shortest && s;
                }
                4 => {
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
