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
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
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
    type Error = crate::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, shortest, len) = hardy_cbor::decode::parse::<(u64, bool, usize)>(data)
            .map_err(crate::Error::InvalidCBOR)?;
        if !shortest {
            return Err(crate::Error::NotCanonical);
        }
        Ok((value.into(), true, len))
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
    if matches!(crc_type, CrcType::None) {
        return Ok(data);
    }
    let mut digest = Digest::new(crc_type)?;
    data.push(digest.cbor_head());
    digest.push(&data);
    digest.push_zeros();
    data.extend_from_slice(&digest.finalize());
    Ok(data)
}

/// Incremental CRC builder/verifier.
///
/// Owns the per-CRC-type facts ({algorithm, value length, big-endian
/// finalisation}) so the rest of the crate doesn't duplicate them.
/// Construct with [`Digest::new`] (errors for [`CrcType::None`] and
/// unrecognised types), then push the block bytes through
/// [`push`](Self::push) — substituting [`push_zeros`](Self::push_zeros)
/// where the CRC value's own bytes live in the wire form. The parse path
/// checks the result against the wire-form value with
/// [`verify`](Self::verify) (no allocation); the emit path retrieves the
/// computed bytes with [`finalize`](Self::finalize) and appends them.
/// Both consume the digest, so each instance is used exactly once.
pub struct Digest {
    state: DigestState,
}

enum DigestState {
    Crc16(::crc::Digest<'static, u16>),
    Crc32(::crc::Digest<'static, u32>),
}

impl Digest {
    /// Constructs a new digest for the given CRC type. Errors for
    /// [`CrcType::None`] (no CRC is expected — don't create a digest)
    /// and [`CrcType::Unrecognised`] (unknown wire-form code).
    pub fn new(crc_type: CrcType) -> Result<Self, Error> {
        let state = match crc_type {
            CrcType::CRC16_X25 => DigestState::Crc16(X25.digest()),
            CrcType::CRC32_CASTAGNOLI => DigestState::Crc32(CASTAGNOLI.digest()),
            CrcType::None => return Err(Error::UnexpectedCrcValue),
            CrcType::Unrecognised(t) => return Err(Error::InvalidType(t)),
        };
        Ok(Self { state })
    }

    /// CBOR byte-string head byte for the wire-form CRC value
    /// (`0x42` for CRC-16, `0x44` for CRC-32). Per RFC 9171 §4.2.2 the
    /// CRC is always encoded as a definite-length byte string of the
    /// type-specific width.
    pub fn cbor_head(&self) -> u8 {
        match self.state {
            DigestState::Crc16(_) => 0x42,
            DigestState::Crc32(_) => 0x44,
        }
    }

    /// Pushes bytes into the running digest.
    pub fn push(&mut self, data: &[u8]) {
        match &mut self.state {
            DigestState::Crc16(d) => d.update(data),
            DigestState::Crc32(d) => d.update(data),
        }
    }

    /// Pushes the CRC value's width in zero bytes (2 for CRC-16, 4 for
    /// CRC-32) into the running digest. Used at the position where the
    /// wire-form CRC value lives — the parse path then compares its
    /// actual bytes via [`verify`](Self::verify); the emit path retrieves
    /// the computed bytes via [`finalize`](Self::finalize). Returns the
    /// number of zero bytes pushed for offset arithmetic.
    pub fn push_zeros(&mut self) -> usize {
        match self.state {
            DigestState::Crc16(_) => {
                self.push(&[0; 2]);
                2
            }
            DigestState::Crc32(_) => {
                self.push(&[0; 4]);
                4
            }
        }
    }

    /// Finalises the digest and checks the computed CRC value against an
    /// `expected` wire-form value bytestring, returning `true` on a match.
    /// This is the parse path: the comparison is made against the
    /// big-endian bytes on the stack, so — unlike [`finalize`](Self::finalize)
    /// — nothing is allocated. A length mismatch compares as unequal.
    pub fn verify(self, expected: &[u8]) -> bool {
        match self.state {
            DigestState::Crc16(d) => expected == d.finalize().to_be_bytes(),
            DigestState::Crc32(d) => expected == d.finalize().to_be_bytes(),
        }
    }

    /// Finalises the digest and returns the computed CRC value as
    /// big-endian bytes (2 for CRC-16, 4 for CRC-32). This is the emit
    /// path; the parse path checks against the wire-form value with
    /// [`verify`](Self::verify) instead, which avoids the allocation.
    pub fn finalize(self) -> Vec<u8> {
        match self.state {
            DigestState::Crc16(d) => d.finalize().to_be_bytes().to_vec(),
            DigestState::Crc32(d) => d.finalize().to_be_bytes().to_vec(),
        }
    }
}
