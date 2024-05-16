use super::*;
use thiserror::Error;

const X25: ::crc::Crc<u16> = ::crc::Crc::<u16>::new(&::crc::CRC_16_IBM_SDLC);
const CASTAGNOLI: ::crc::Crc<u32> = ::crc::Crc::<u32>::new(&::crc::CRC_32_ISCSI);

#[derive(Error, Debug)]
pub enum Error {
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

pub fn parse_crc_value(
    data: &[u8],
    block_start: usize,
    block: &mut cbor::decode::Array,
    crc_type: CrcType,
) -> Result<(), Error> {
    // Parse CRC
    let crc_value = block.try_parse_value(|value, crc_start, tags| match value {
        cbor::decode::Value::Bytes(crc, _) => {
            if !tags.is_empty() {
                log::trace!("Parsing bundle block CRC value with tags");
            }
            match crc_type {
                CrcType::None => Err(Error::UnexpectedCrcValue),
                CrcType::CRC16_X25 => {
                    if crc.len() != 2 {
                        Err(Error::InvalidLength(crc.len()))
                    } else {
                        Ok((
                            u16::from_be_bytes(crc.try_into().unwrap()) as u32,
                            crc_start,
                        ))
                    }
                }
                CrcType::CRC32_CASTAGNOLI => {
                    if crc.len() != 4 {
                        Err(Error::InvalidLength(crc.len()))
                    } else {
                        Ok((u32::from_be_bytes(crc.try_into().unwrap()), crc_start))
                    }
                }
            }
        }
        _ => Err(
            cbor::decode::Error::IncorrectType("Byte String".to_string(), value.type_name()).into(),
        ),
    })?;

    // Confirm we are at the end of the block
    let Some(block_end) = block.end()? else {
        return Err(Error::AdditionalItems);
    };

    // Now check CRC
    if let Some(((crc_value, crc_start), crc_end)) = crc_value {
        match crc_type {
            CrcType::CRC16_X25 => {
                let mut digest = X25.digest();
                digest.update(&data[block_start..crc_start]);
                digest.update(&vec![0; crc_end - crc_start]);
                if block_end > crc_end {
                    digest.update(&data[crc_end..block_end]);
                }
                if crc_value != digest.finalize() as u32 {
                    return Err(Error::IncorrectCrc);
                }
            }
            CrcType::CRC32_CASTAGNOLI => {
                let mut digest = CASTAGNOLI.digest();
                digest.update(&data[block_start..crc_start]);
                digest.update(&vec![0; crc_end - crc_start]);
                if block_end > crc_end {
                    digest.update(&data[crc_end..block_end]);
                }
                if crc_value != digest.finalize() {
                    return Err(Error::IncorrectCrc);
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
