use super::*;
use core::ops::Range;
use error::CaptureFieldErr;
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct Flags {
    pub must_replicate: bool,
    pub report_on_failure: bool,
    pub delete_bundle_on_failure: bool,
    pub delete_block_on_failure: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
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
        let mut flags = Self {
            unrecognised: {
                let u = value & !((2 ^ 6) - 1);
                if u == 0 { None } else { Some(u) }
            },
            ..Default::default()
        };

        for b in 0..=6 {
            if value & (1 << b) != 0 {
                match b {
                    0 => flags.must_replicate = true,
                    1 => flags.report_on_failure = true,
                    2 => flags.delete_bundle_on_failure = true,
                    4 => flags.delete_block_on_failure = true,
                    b => {
                        flags.unrecognised = Some(flags.unrecognised.unwrap_or_default() | 1 << b);
                    }
                }
            }
        }
        flags
    }
}

impl hardy_cbor::encode::ToCbor for Flags {
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(&u64::from(self))
    }
}

impl hardy_cbor::decode::FromCbor for Flags {
    type Error = hardy_cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data)
            .map(|o| o.map(|(value, shortest, len)| (value.into(), shortest, len)))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Type {
    Primary,
    Payload,
    PreviousNode,
    BundleAge,
    HopCount,
    BlockIntegrity,
    BlockSecurity,
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
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(&u64::from(*self))
    }
}

impl hardy_cbor::decode::FromCbor for Type {
    type Error = hardy_cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data)
            .map(|o| o.map(|(value, shortest, len)| (value.into(), shortest, len)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub block_type: Type,
    pub flags: Flags,
    pub crc_type: crc::CrcType,
    pub extent: Range<usize>,
    pub data: Range<usize>,
    pub bib: Option<u64>,
    pub bcb: Option<u64>,
}

impl Block {
    pub fn payload(&self) -> Range<usize> {
        self.extent.start + self.data.start..self.extent.start + self.data.end
    }

    pub(crate) fn emit(
        &mut self,
        block_number: u64,
        data: &[u8],
        array: &mut hardy_cbor::encode::Array,
    ) {
        let block_data = crc::append_crc_value(
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

                    self.data = a.emit_measured(data);

                    // CRC
                    if let crc::CrcType::None = self.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        );
        let block_start = array.offset();
        array.emit_raw(block_data);
        self.extent = block_start..array.offset();
    }

    pub(crate) fn r#move(&mut self, source_data: &[u8], array: &mut hardy_cbor::encode::Array) {
        let block_start = array.offset();
        array.emit_raw_slice(&source_data[self.extent.clone()]);
        self.extent = block_start..array.offset();
    }
}

#[derive(Clone)]
pub(crate) struct BlockWithNumber {
    pub number: u64,
    pub block: Block,
    pub payload: Option<Box<[u8]>>,
}

impl hardy_cbor::decode::FromCbor for BlockWithNumber {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse_array(data, |arr, mut shortest, tags| {
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
        .map(|o| {
            o.map(|((mut block, shortest), len)| {
                block.block.extent.end = len;
                (block, shortest, len)
            })
        })
    }
}
