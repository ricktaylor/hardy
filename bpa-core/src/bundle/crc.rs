use super::*;
use thiserror::Error;

#[allow(non_camel_case_types)]
#[derive(Default, Debug, Copy, Clone)]
pub enum CrcType {
    #[default]
    None = 0,
    CRC16_X25 = 1,
    CRC32_CASTAGNOLI = 2,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Invalid CRC Type {0}")]
    InvalidType(u64),
}

#[derive(Error, Debug)]
pub enum DecodeError {
    #[error(transparent)]
    InvalidType(#[from] Error),

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),
}

impl TryFrom<u64> for CrcType {
    type Error = Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::CRC16_X25),
            2 => Ok(Self::CRC32_CASTAGNOLI),
            _ => Err(Error::InvalidType(value)),
        }
    }
}

impl From<CrcType> for u64 {
    fn from(value: CrcType) -> Self {
        value as u64
    }
}

impl cbor::decode::FromCbor for CrcType {
    type Error = DecodeError;

    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), Self::Error> {
        let (code, len, tags) = cbor::decode::parse_detail::<u64>(data)?;
        Ok((code.try_into()?, len, tags))
    }
}

impl cbor::encode::ToCbor for CrcType {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit::<u64>(self.into())
    }
}
