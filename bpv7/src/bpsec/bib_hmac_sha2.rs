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

impl From<ShaVariant> for u64 {
    fn from(value: ShaVariant) -> Self {
        match value {
            ShaVariant::HMAC_256_256 => 5,
            ShaVariant::HMAC_384_384 => 6,
            ShaVariant::HMAC_512_512 => 7,
            ShaVariant::Unrecognised(v) => v,
        }
    }
}

impl From<u64> for ShaVariant {
    fn from(value: u64) -> Self {
        match value {
            5 => ShaVariant::HMAC_256_256,
            6 => ShaVariant::HMAC_384_384,
            7 => ShaVariant::HMAC_512_512,
            v => ShaVariant::Unrecognised(v),
        }
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

impl From<u64> for ScopeFlags {
    fn from(value: u64) -> Self {
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
        flags
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
                    let (variant, s) =
                        cbor::decode::parse::<(u64, bool)>(&data[range.start..range.end])?;
                    result.variant = variant.into();
                    shortest = shortest && s;
                }
                2 => {
                    let (key, s) =
                        cbor::decode::parse::<(Box<[u8]>, bool)>(&data[range.start..range.end])?;
                    result.key = Some(key);
                    shortest = shortest && s;
                }
                3 => {
                    let (flags, s) =
                        cbor::decode::parse::<(u64, bool)>(&data[range.start..range.end])?;
                    result.flags = flags.into();
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