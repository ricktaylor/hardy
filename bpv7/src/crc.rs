/*!
This module provides functionality for handling Cyclic Redundancy Checks (CRCs)
for bundles, as specified in RFC 9171. It supports different CRC types and
provides functions for parsing and validating CRCs from incoming bundles, as
well as appending CRCs to outgoing bundles.
*/

use super::*;
use thiserror::Error;

const X25: ::crc::Crc<u16> = ::crc::Crc::<u16>::new(&::crc::CRC_16_IBM_SDLC);
const CASTAGNOLI: ::crc::Crc<u32> = ::crc::Crc::<u32>::new(&::crc::CRC_32_ISCSI);

/// Errors that can occur during CRC processing.
#[derive(Error, Debug)]
pub enum Error {
    /// Indicates that an invalid or unsupported CRC type was specified.
    #[error("Invalid CRC Type {0}")]
    InvalidType(u64),

    /// Indicates that the CRC value in a block has an unexpected length.
    #[error("Block has unexpected CRC value length {0}")]
    InvalidLength(usize),

    /// Indicates that a block has a CRC value but no CRC type was specified.
    #[error("Block has a CRC value with no CRC type specified")]
    UnexpectedCrcValue,

    /// Indicates that the calculated CRC value does not match the one in the block.
    #[error("Incorrect CRC value")]
    IncorrectCrc,

    /// Indicates that a CRC value was expected but not found.
    #[error("Missing CRC value")]
    MissingCrc,

    /// An error occurred during CBOR decoding.
    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),
}

/// Represents the type of CRC used in a bundle block.
#[allow(non_camel_case_types)]
#[derive(Default, Debug, Copy, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CrcType {
    /// No CRC is used.
    #[default]
    None,
    /// CRC-16/X-25.
    CRC16_X25,
    /// CRC-32/Castagnoli.
    CRC32_CASTAGNOLI,
    /// An unrecognized CRC type.
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

/// Parses and validates the CRC value of a block.
///
/// This function is intended for internal use by the bundle parsing logic.
/// It reads the CRC value from the block, calculates the CRC of the block's content,
/// and compares the two to ensure data integrity.
///
/// # Arguments
/// * `data` - The raw byte slice of the entire block.
/// * `block` - A mutable reference to the CBOR array decoder for the block.
/// * `crc_type` - The type of CRC to use for validation.
///
/// # Returns
/// A `Result` containing a boolean indicating if the CBOR encoding was in its shortest form, or an `Error` if validation fails.
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
            Err(Error::InvalidCBOR(
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
            if crc.start > 0 {
                digest.update(&data[0..crc.start]);
            }
            digest.update(&[0u8; 2]);
            if crc_end > crc.end {
                digest.update(&data[crc.end..crc_end]);
            }
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
            if crc.start > 0 {
                digest.update(&data[0..crc.start]);
            }
            digest.update(&[0u8; 4]);
            if crc_end > crc.end {
                digest.update(&data[crc.end..crc_end]);
            }
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

/// Appends a CRC value to a block's data.
///
/// This function is intended for internal use when creating a bundle.
/// It calculates the CRC of the provided data and appends the CRC value
/// in the correct format.
///
/// # Arguments
/// * `crc_type` - The type of CRC to append.
/// * `data` - The data to which the CRC will be appended.
///
/// # Returns
/// A `Result` containing the data with the appended CRC, or an `Error` if the CRC type is invalid.
pub(super) fn append_crc_value(crc_type: CrcType, mut data: Vec<u8>) -> Result<Vec<u8>, Error> {
    match crc_type {
        CrcType::None => {}
        CrcType::CRC16_X25 => {
            // Append CBOR byte string header for a 2-byte string
            data.push(0x42);
            // Calculate CRC over the data so far, plus a 2-byte zero placeholder
            let mut digest = X25.digest();
            digest.update(&data);
            digest.update(&[0; 2]);
            // Append the final calculated CRC
            data.extend_from_slice(&digest.finalize().to_be_bytes());
        }
        CrcType::CRC32_CASTAGNOLI => {
            // Append CBOR byte string header for a 4-byte string
            data.push(0x44);
            // Calculate CRC over the data so far, plus a 4-byte zero placeholder
            let mut digest = CASTAGNOLI.digest();
            digest.update(&data);
            digest.update(&[0; 4]);
            // Append the final calculated CRC
            data.extend_from_slice(&digest.finalize().to_be_bytes());
        }
        CrcType::Unrecognised(t) => return Err(Error::InvalidType(t)),
    }
    Ok(data)
}
