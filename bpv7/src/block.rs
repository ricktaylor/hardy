/*!
This module defines the structure and components of a BPv7 block, which is the
fundamental unit of a bundle. It includes definitions for block headers, flags,
and the generic `Block` struct that represents all extension blocks.
*/

use super::*;
use core::ops::Range;
use error::CaptureFieldErr;

/// Represents the processing control flags for a BPv7 block.
///
/// These flags, defined in RFC 9171 Section 4.2.2, control how a node should
/// process the block, especially in cases of failure or fragmentation.
#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct Flags {
    /// If set, the block must be replicated in every fragment of the bundle.
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub must_replicate: bool,
    /// If set, a status report should be generated if block processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub report_on_failure: bool,
    /// If set, the entire bundle should be deleted if block processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delete_bundle_on_failure: bool,
    /// If set, this block should be deleted if its processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delete_block_on_failure: bool,

    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    /// A bitmask of any unrecognized flags encountered during parsing.
    pub unrecognised: Option<u64>,
}

impl From<&Flags> for u64 {
    fn from(value: &Flags) -> Self {
        let mut flags = value.unrecognised.unwrap_or_default();
        if value.must_replicate {
            flags |= 1 << 0;
        }
        if value.report_on_failure {
            flags |= 1 << 1;
        }
        if value.delete_bundle_on_failure {
            flags |= 1 << 2;
        }
        if value.delete_block_on_failure {
            flags |= 1 << 4;
        }
        flags
    }
}

impl From<u64> for Flags {
    fn from(value: u64) -> Self {
        let mut flags = Self::default();
        let mut unrecognised = value;

        if (value & 1) != 0 {
            flags.must_replicate = true;
            unrecognised &= !1;
        }
        if (value & 2) != 0 {
            flags.report_on_failure = true;
            unrecognised &= !2;
        }
        if (value & 4) != 0 {
            flags.delete_bundle_on_failure = true;
            unrecognised &= !4;
        }
        if (value & 16) != 0 {
            flags.delete_block_on_failure = true;
            unrecognised &= !16;
        }

        if unrecognised != 0 {
            flags.unrecognised = Some(unrecognised);
        }
        flags
    }
}

impl hardy_cbor::encode::ToCbor for Flags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&u64::from(self))
    }
}

impl hardy_cbor::decode::FromCbor for Flags {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data)
            .map(|(value, shortest, len)| (value.into(), shortest, len))
    }
}

/// The type of a BPv7 block, as defined in RFC 9171 Section 4.2.1.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub enum Type {
    /// Primary Block (type code 0).
    Primary,
    /// Payload Block (type code 1).
    Payload,
    /// Previous Node Block (type code 6).
    PreviousNode,
    /// Bundle Age Block (type code 7).
    BundleAge,
    /// Hop Count Block (type code 10).
    HopCount,
    /// Block Integrity Block (from BPSec, RFC 9172).
    BlockIntegrity,
    /// Block Confidentiality Block (from BPSec, RFC 9172).
    BlockSecurity,
    /// An unrecognized block type with its type code.
    Unrecognised(u64),
}

impl From<Type> for u64 {
    fn from(value: Type) -> Self {
        match value {
            Type::Primary => 0,
            Type::Payload => 1,
            Type::PreviousNode => 6,
            Type::BundleAge => 7,
            Type::HopCount => 10,
            Type::BlockIntegrity => 11,
            Type::BlockSecurity => 12,
            Type::Unrecognised(v) => v,
        }
    }
}

impl From<u64> for Type {
    fn from(value: u64) -> Self {
        match value {
            0 => Type::Primary,
            1 => Type::Payload,
            6 => Type::PreviousNode,
            7 => Type::BundleAge,
            10 => Type::HopCount,
            11 => Type::BlockIntegrity,
            12 => Type::BlockSecurity,
            value => Type::Unrecognised(value),
        }
    }
}

impl hardy_cbor::encode::ToCbor for Type {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&u64::from(*self))
    }
}

impl hardy_cbor::decode::FromCbor for Type {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data)
            .map(|(value, shortest, len)| (value.into(), shortest, len))
    }
}

/// Represents the payload of a block.
///
/// The payload can either be a direct slice (`Borrowed`) into the original bundle's
/// byte array, or an `Owned` byte slice. The `Owned` variant is used when the
/// payload has been decrypted from a Block Confidentiality Block (BCB) and
/// therefore does not correspond to a contiguous region of the original data.
pub enum Payload<'a> {
    /// A slice within the original bundle data.
    Borrowed(&'a [u8]),
    /// An owned byte slice, typically holding a decrypted payload.
    Owned(zeroize::Zeroizing<Box<[u8]>>),
}

impl core::fmt::Debug for Payload<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Just delegate to the underlying slice formatter
        self.as_ref().fmt(f)
    }
}

impl AsRef<[u8]> for Payload<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Borrowed(arg0) => arg0,
            Self::Owned(arg0) => arg0.as_ref(),
        }
    }
}

/// Represents a generic BPv7 extension block within a bundle.
///
/// This struct holds the common metadata for all blocks, such as the type, flags,
/// and CRC information. The actual data of the block is not stored directly but
/// is referenced by the `extent` and `data` ranges, which point to slices
/// within the full bundle's byte representation.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct Block {
    /// The type of the block.
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub block_type: Type,
    /// The block-specific processing control flags.
    pub flags: Flags,
    /// The type of CRC used for this block's integrity check.
    pub crc_type: crc::CrcType,
    /// The block number of the Block Integrity Block (BIB) that protects this block, if any.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub bib: Option<u64>,
    /// The block number of the Block Confidentiality Block (BCB) that protects this block, if any.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub bcb: Option<u64>,
    /// The range of bytes in the source data that this block occupies, including the CBOR array wrapper.
    pub extent: Range<usize>,
    /// The range of bytes within the `extent` that represents the block-specific data.
    pub data: Range<usize>,
}

impl Block {
    /// Calculates the absolute range of the block's payload within the original bundle byte array.
    pub fn payload_range(&self) -> Range<usize> {
        self.extent.start + self.data.start..self.extent.start + self.data.end
    }

    /// Emits the block as a CBOR-encoded byte array.
    /// This is an internal function used during bundle creation.
    pub(crate) fn emit(
        &mut self,
        block_number: u64,
        data: &[u8],
        array: &mut hardy_cbor::encode::Array,
    ) -> Result<(), Error> {
        self.extent = array.emit(&hardy_cbor::encode::Raw(&crc::append_crc_value(
            self.crc_type,
            hardy_cbor::encode::emit_array(
                Some(if let crc::CrcType::None = self.crc_type {
                    5
                } else {
                    6
                }),
                |a| {
                    a.emit(&self.block_type);
                    a.emit(&block_number);
                    a.emit(&self.flags);
                    a.emit(&self.crc_type);

                    self.data = a.emit(&hardy_cbor::encode::Bytes(data));

                    // CRC
                    if let crc::CrcType::None = self.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        )?));
        Ok(())
    }

    /// Copies the entire block from a source byte array to a new CBOR array.
    /// This is an internal function used when modifying a bundle.
    pub(crate) fn copy_whole(&mut self, source_data: &[u8], array: &mut hardy_cbor::encode::Array) {
        self.extent = array.emit(&hardy_cbor::encode::Raw(&source_data[self.extent.clone()]));
    }
}

/// A helper struct used during bundle parsing to associate a block with its block number.
#[derive(Clone)]
pub(crate) struct BlockWithNumber {
    /// The block number.
    pub number: u64,
    /// The block itself.
    pub block: Block,
    /// The block's payload, if it was parsed from an indefinite-length byte string.
    pub payload: Option<Box<[u8]>>,
}

impl hardy_cbor::decode::FromCbor for BlockWithNumber {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |arr, mut shortest, tags| {
            shortest = shortest && tags.is_empty() && arr.is_definite();

            let block_type = arr
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("block type code")?;

            let block_number = arr.parse().map_field_err("block number").map(|(v, s)| {
                shortest = shortest && s;
                v
            })?;
            match (block_number, block_type) {
                (1, Type::Payload) => {}
                (0, _) | (1, _) | (_, Type::Primary) | (_, Type::Payload) => {
                    return Err(Error::InvalidBlockNumber(block_number, block_type));
                }
                _ => {}
            }

            let flags = arr
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("block processing control flags")?;

            let crc_type = arr
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("CRC type")?;

            // Stash start of data
            let payload_start = arr.offset();
            let (payload, payload_range) = arr.parse_value(|value, s, tags| {
                shortest = shortest && s;
                if shortest {
                    // Appendix B of RFC9171
                    let mut seen_24 = false;
                    for tag in tags {
                        match *tag {
                            24 if !seen_24 => seen_24 = true,
                            _ => {
                                shortest = false;
                                break;
                            }
                        }
                    }
                }

                match value {
                    hardy_cbor::decode::Value::Bytes(r) => {
                        Ok((None, payload_start + r.start..payload_start + r.end))
                    }
                    hardy_cbor::decode::Value::ByteStream(ranges) => {
                        shortest = false;
                        Ok((
                            Some(
                                ranges
                                    .into_iter()
                                    .fold(Vec::new(), |mut acc, r| {
                                        acc.extend_from_slice(&data[r]);
                                        acc
                                    })
                                    .into(),
                            ),
                            0..0,
                        ))
                    }
                    value => Err(hardy_cbor::decode::Error::IncorrectType(
                        "Byte String".to_string(),
                        value.type_name(!tags.is_empty()),
                    )),
                }
            })?;

            // Check CRC
            shortest = crc::parse_crc_value(data, arr, crc_type)? && shortest;

            Ok((
                BlockWithNumber {
                    number: block_number,
                    block: Block {
                        block_type,
                        flags,
                        crc_type,
                        extent: 0..0,
                        data: payload_range,
                        bib: None,
                        bcb: None,
                    },
                    payload,
                },
                shortest,
            ))
        })
        .map(|((mut block, shortest), len)| {
            block.block.extent.end = len;
            (block, shortest, len)
        })
    }
}
