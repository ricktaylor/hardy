use thiserror::Error;

const X25: ::crc::Crc<u16> = ::crc::Crc::<u16>::new(&::crc::CRC_16_IBM_SDLC);
const CASTAGNOLI: ::crc::Crc<u32> = ::crc::Crc::<u32>::new(&::crc::CRC_32_ISCSI);

#[derive(Error, Debug)]
pub enum Error {
    #[error("Invalid CRC Type {0}")]
    InvalidType(u64),

    #[error("Block has unexpected CRC value length {0}")]
    InvalidLength(usize),

    #[error("Block has a CRC value with no CRC type specified")]
    UnexpectedCrcValue,

    #[error("Incorrect CRC value")]
    IncorrectCrc,

    #[error("Missing CRC value")]
    MissingCrc,

    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),
}

#[allow(non_camel_case_types)]
#[derive(Default, Debug, Copy, Clone)]
pub enum CrcType {
    #[default]
    None,
    CRC16_X25,
    CRC32_CASTAGNOLI,
    Unrecognised(u64),
}

impl From<u64> for CrcType {
    fn from(value: u64) -> Self {
        match value {
            0 => Self::None,
            1 => Self::CRC16_X25,
            2 => Self::CRC32_CASTAGNOLI,
            v => Self::Unrecognised(v),
        }
    }
}

impl From<CrcType> for u64 {
    fn from(value: CrcType) -> Self {
        match value {
            CrcType::None => 0,
            CrcType::CRC16_X25 => 1,
            CrcType::CRC32_CASTAGNOLI => 2,
            CrcType::Unrecognised(v) => v,
        }
    }
}

impl hardy_cbor::encode::ToCbor for CrcType {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(u64::from(self))
    }
}

impl hardy_cbor::decode::FromCbor for CrcType {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data)
            .map(|o| o.map(|(v, shortest, len)| (v.into(), shortest, len)))
            .map_err(Into::into)
    }
}

pub fn parse_crc_value(
    data: &[u8],
    block: &mut hardy_cbor::decode::Array,
    crc_type: CrcType,
) -> Result<bool, Error> {
    // Parse CRC
    let crc_value = block.try_parse_value(|value, shortest, tags| match value {
        hardy_cbor::decode::Value::Bytes(crc) => match crc_type {
            CrcType::None => Err(Error::UnexpectedCrcValue),
            CrcType::CRC16_X25 => {
                if crc.len() != 2 {
                    Err(Error::InvalidLength(crc.len()))
                } else {
                    Ok((
                        u16::from_be_bytes(crc.try_into().unwrap()) as u32,
                        shortest && tags.is_empty(),
                    ))
                }
            }
            CrcType::CRC32_CASTAGNOLI => {
                if crc.len() != 4 {
                    Err(Error::InvalidLength(crc.len()))
                } else {
                    Ok((
                        u32::from_be_bytes(crc.try_into().unwrap()),
                        shortest && tags.is_empty(),
                    ))
                }
            }
            CrcType::Unrecognised(_) => Ok((0, true)),
        },
        _ => Err(hardy_cbor::decode::Error::IncorrectType(
            "Definite-length Byte String".to_string(),
            value.type_name(!tags.is_empty()),
        )
        .into()),
    })?;

    let crc_val_end = block.offset();
    let crc_end = block.end()?.unwrap_or(crc_val_end);

    // Now check CRC
    match (crc_type, crc_value) {
        (CrcType::None, None) => Ok(true),
        (CrcType::CRC16_X25, Some((crc_value, shortest))) => {
            let mut digest = X25.digest();
            digest.update(&data[0..crc_val_end - 2]);
            digest.update(&[0u8; 2]);
            digest.update(&data[crc_val_end..crc_end]);
            if crc_value != digest.finalize() as u32 {
                Err(Error::IncorrectCrc)
            } else {
                Ok(shortest)
            }
        }
        (CrcType::CRC32_CASTAGNOLI, Some((crc_value, shortest))) => {
            let mut digest = CASTAGNOLI.digest();
            digest.update(&data[0..crc_val_end - 4]);
            digest.update(&[0u8; 4]);
            digest.update(&data[crc_val_end..crc_end]);
            if crc_value != digest.finalize() {
                Err(Error::IncorrectCrc)
            } else {
                Ok(shortest)
            }
        }
        (CrcType::Unrecognised(t), _) => Err(Error::InvalidType(t)),
        _ => Err(Error::MissingCrc),
    }
}

pub fn append_crc_value(crc_type: CrcType, mut data: Vec<u8>) -> Vec<u8> {
    match crc_type {
        CrcType::None => {}
        CrcType::CRC16_X25 => {
            data.push(0x42);
            let mut digest = X25.digest();
            digest.update(&data);
            digest.update(&[0; 2]);
            data.extend_from_slice(&digest.finalize().to_be_bytes());
        }
        CrcType::CRC32_CASTAGNOLI => {
            data.push(0x44);
            let mut digest = CASTAGNOLI.digest();
            digest.update(&data);
            digest.update(&[0; 4]);
            data.extend_from_slice(&digest.finalize().to_be_bytes());
        }
        _ => unreachable!(),
    }
    data
}
