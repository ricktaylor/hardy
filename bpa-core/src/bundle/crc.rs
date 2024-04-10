use super::*;

#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Default)]
pub enum CrcType {
    #[default]
    None = 0,
    CRC16_X25 = 1,
    CRC32_CASTAGNOLI = 2,
}

impl TryFrom<u64> for CrcType {
    type Error = anyhow::Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::CRC16_X25),
            2 => Ok(Self::CRC32_CASTAGNOLI),
            _ => Err(anyhow!("Invalid CRC type {}", value)),
        }
    }
}

impl From<CrcType> for u64 {
    fn from(value: CrcType) -> Self {
        value as u64
    }
}

impl cbor::decode::FromCbor for CrcType {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (code, o, tags) = cbor::decode::parse_detail::<u64>(data)?;
        Ok((code.try_into()?, o, tags))
    }
}

impl cbor::encode::ToCbor for CrcType {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        cbor::encode::emit_with_tags::<u64>(self.into(), tags)
    }
}
