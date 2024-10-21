use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Default)]
pub enum AesVariant {
    A128GCM,
    #[default]
    A256GCM,
    Unrecognised(u64),
}

impl From<AesVariant> for u64 {
    fn from(value: AesVariant) -> Self {
        match value {
            AesVariant::A128GCM => 1,
            AesVariant::A256GCM => 3,
            AesVariant::Unrecognised(v) => v,
        }
    }
}

impl From<u64> for AesVariant {
    fn from(value: u64) -> Self {
        match value {
            1 => AesVariant::A128GCM,
            3 => AesVariant::A256GCM,
            v => AesVariant::Unrecognised(v),
        }
    }
}

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

#[derive(Default)]
pub struct Parameters {
    pub iv: Option<Box<[u8]>>,
    pub variant: AesVariant,
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
                    let (iv, s) =
                        cbor::decode::parse::<(Box<[u8]>, bool)>(&data[range.start..range.end])?;
                    result.iv = Some(iv);
                    shortest = shortest && s;
                }
                2 => {
                    let (variant, s) =
                        cbor::decode::parse::<(u64, bool)>(&data[range.start..range.end])?;
                    result.variant = variant.into();
                    shortest = shortest && s;
                }
                3 => {
                    let (key, s) =
                        cbor::decode::parse::<(Box<[u8]>, bool)>(&data[range.start..range.end])?;
                    result.key = Some(key);
                    shortest = shortest && s;
                }
                4 => {
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

#[derive(Default)]
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
