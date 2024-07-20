use super::*;
use thiserror::Error;

const X25: ::crc::Crc<u16> = ::crc::Crc::<u16>::new(&::crc::CRC_16_IBM_SDLC);
const CASTAGNOLI: ::crc::Crc<u32> = ::crc::Crc::<u32>::new(&::crc::CRC_32_ISCSI);

#[derive(Error, Debug)]
pub enum CrcError {
    #[error("Invalid CRC Type {0}")]
    InvalidType(u64),

    #[error("Block has unexpected CRC value length {0}")]
    InvalidLength(usize),

    #[error("Block has a CRC value with no CRC type specified")]
    UnexpectedCrcValue,

    #[error("Block has additional items after CRC value")]
    AdditionalItems,

    #[error("Incorrect CRC value")]
    IncorrectCrc,

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),
}

#[allow(non_camel_case_types)]
#[derive(Default, Debug, Copy, Clone)]
pub enum CrcType {
    #[default]
    None = 0,
    CRC16_X25 = 1,
    CRC32_CASTAGNOLI = 2,
}

impl TryFrom<u64> for CrcType {
    type Error = CrcError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::CRC16_X25),
            2 => Ok(Self::CRC32_CASTAGNOLI),
            _ => Err(CrcError::InvalidType(value)),
        }
    }
}

impl From<CrcType> for u64 {
    fn from(value: CrcType) -> Self {
        value as u64
    }
}

impl cbor::decode::FromCbor for CrcType {
    type Error = CrcError;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((v, len, tags)) = u64::try_from_cbor_tagged(data)? {
            Ok(Some((v.try_into()?, len, tags)))
        } else {
            Ok(None)
        }
    }
}

pub fn parse_crc_value(
    data: &[u8],
    block: &mut cbor::decode::Array,
    crc_type: CrcType,
) -> Result<(), CrcError> {
    // Parse CRC
    let crc_value = block.try_parse_value(|value, crc_start, tags| match value {
        cbor::decode::Value::Bytes(crc, _) => {
            if !tags.is_empty() {
                trace!("Parsing bundle block CRC value with tags");
            }
            match crc_type {
                CrcType::None => Err(CrcError::UnexpectedCrcValue),
                CrcType::CRC16_X25 => {
                    if crc.len() != 2 {
                        Err(CrcError::InvalidLength(crc.len()))
                    } else {
                        Ok((
                            u16::from_be_bytes(crc.try_into().unwrap()) as u32,
                            crc_start + 1,
                        ))
                    }
                }
                CrcType::CRC32_CASTAGNOLI => {
                    if crc.len() != 4 {
                        Err(CrcError::InvalidLength(crc.len()))
                    } else {
                        Ok((u32::from_be_bytes(crc.try_into().unwrap()), crc_start + 1))
                    }
                }
            }
        }
        _ => Err(
            cbor::decode::Error::IncorrectType("Byte String".to_string(), value.type_name()).into(),
        ),
    })?;

    // Now check CRC
    if let Some(((crc_value, crc_start), _)) = crc_value {
        match crc_type {
            CrcType::CRC16_X25 => {
                let mut digest = X25.digest();

                // TODO: Need to confirm this logic is correct
                digest.update(&data[0..crc_start]);
                digest.update(&[0u8; 2]);
                digest.update(&data[crc_start + 2..]);
                if crc_value != digest.finalize() as u32 {
                    return Err(CrcError::IncorrectCrc);
                }
            }
            CrcType::CRC32_CASTAGNOLI => {
                let mut digest = CASTAGNOLI.digest();
                digest.update(&data[0..crc_start]);
                digest.update(&[0u8; 4]);
                digest.update(&data[crc_start + 4..]);
                if crc_value != digest.finalize() {
                    return Err(CrcError::IncorrectCrc);
                }
            }
            CrcType::None => unreachable!(),
        }
    }
    Ok(())
}

pub fn emit_crc_value(crc_type: CrcType, mut data: Vec<u8>) -> Vec<u8> {
    match crc_type {
        CrcType::CRC16_X25 => {
            let crc_value = X25.checksum(&data).to_be_bytes();
            data.truncate(data.len() - crc_value.len());
            data.extend(crc_value)
        }
        CrcType::CRC32_CASTAGNOLI => {
            let crc_value = CASTAGNOLI.checksum(&data).to_be_bytes();
            data.truncate(data.len() - crc_value.len());
            data.extend(crc_value)
        }
        CrcType::None => {}
    }
    data
}
