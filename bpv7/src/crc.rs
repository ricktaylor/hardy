use super::*;
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
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
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&u64::from(*self))
    }
}

impl hardy_cbor::decode::FromCbor for CrcType {
    type Error = self::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data)
            .map(|(v, shortest, len)| (v.into(), shortest, len))
            .map_err(Into::into)
    }
}

pub(super) fn parse_crc_value(
    data: &[u8],
    block: &mut hardy_cbor::decode::Array,
    crc_type: CrcType,
) -> Result<bool, Error> {
    // Parse CRC
    let crc_start = block.offset();
    let crc_value = block.try_parse_value(|value, shortest, tags| {
        if let hardy_cbor::decode::Value::Bytes(crc) = value {
            Ok((
                crc.start + crc_start..crc.end + crc_start,
                shortest && tags.is_empty(),
            ))
        } else {
            Err(crc::Error::InvalidCBOR(
                hardy_cbor::decode::Error::IncorrectType(
                    "Definite-length Byte String".to_string(),
                    value.type_name(!tags.is_empty()),
                ),
            ))
        }
    })?;
    // Check we are at the end
    block.at_end()?;
    let crc_end = block.offset();

    // Now check CRC
    match (crc_type, crc_value) {
        (CrcType::None, None) => Ok(true),
        (CrcType::None, _) => Err(Error::UnexpectedCrcValue),
        (CrcType::CRC16_X25, Some((crc, shortest))) => {
            let crc_value = u16::from_be_bytes(
                data[crc.start..crc.end]
                    .try_into()
                    .map_err(|_| Error::InvalidLength(crc.len()))?,
            );
            let mut digest = X25.digest();
            digest.update(&data[0..crc.start]);
            digest.update(&[0u8; 2]);
            digest.update(&data[crc.end..crc_end]);
            if crc_value != digest.finalize() {
                Err(Error::IncorrectCrc)
            } else {
                Ok(shortest)
            }
        }
        (CrcType::CRC32_CASTAGNOLI, Some((crc, shortest))) => {
            let crc_value = u32::from_be_bytes(
                data[crc.start..crc.end]
                    .try_into()
                    .map_err(|_| Error::InvalidLength(crc.len()))?,
            );
            let mut digest = CASTAGNOLI.digest();
            digest.update(&data[0..crc.start]);
            digest.update(&[0u8; 4]);
            digest.update(&data[crc.end..crc_end]);
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

pub(super) fn append_crc_value(crc_type: CrcType, mut data: Vec<u8>) -> Vec<u8> {
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
        CrcType::Unrecognised(t) => panic!("Invalid CRC type {t}"),
    }
    data
}
