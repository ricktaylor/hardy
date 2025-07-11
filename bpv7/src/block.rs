use super::*;
use error::CaptureFieldErr;

#[derive(Default, Debug, Clone)]
pub struct Flags {
    pub must_replicate: bool,
    pub report_on_failure: bool,
    pub delete_bundle_on_failure: bool,
    pub delete_block_on_failure: bool,
    pub unrecognised: u64,
}

impl From<&Flags> for u64 {
    fn from(value: &Flags) -> Self {
        let mut flags = value.unrecognised;
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
            unrecognised: value & !((2 ^ 6) - 1),
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
                        flags.unrecognised |= 1 << b;
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

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone)]
pub struct Block {
    pub block_type: Type,
    pub flags: Flags,
    pub crc_type: crc::CrcType,
    pub data_start: usize,
    pub data_len: usize,
    pub payload_offset: usize,
    pub payload_len: usize,
    pub bcb: Option<u64>,
}

impl Block {
    pub fn payload_range(&self) -> core::ops::Range<usize> {
        core::ops::Range {
            start: self.data_start + self.payload_offset,
            end: self.data_start + self.payload_offset + self.payload_len,
        }
    }

    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.payload_range()]
    }

    fn emit_inner(
        &mut self,
        block_number: u64,
        array: &mut hardy_cbor::encode::Array,
        f: impl FnOnce(&mut hardy_cbor::encode::Array),
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

                    // Payload
                    self.payload_offset = a.offset();
                    f(a);
                    self.payload_len = a.offset() - self.payload_offset;

                    // CRC
                    if let crc::CrcType::None = self.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        );
        self.data_start = array.offset();
        self.data_len = block_data.len();
        array.emit_raw(block_data)
    }

    pub(crate) fn emit(
        &mut self,
        block_number: u64,
        data: &[u8],
        array: &mut hardy_cbor::encode::Array,
    ) {
        self.emit_inner(block_number, array, |a| a.emit(data));
    }

    pub(crate) fn rewrite(
        &mut self,
        block_number: u64,
        array: &mut hardy_cbor::encode::Array,
        source_data: &[u8],
    ) {
        hardy_cbor::decode::parse_value(self.payload(source_data), |value, _, _| {
            match value {
                hardy_cbor::decode::Value::Bytes(data) => self.emit(block_number, data, array),
                hardy_cbor::decode::Value::ByteStream(data) => {
                    // This is horrible, but removes a potentially large data copy
                    let len = data.iter().fold(0u64, |len, d| len + d.len() as u64);
                    let mut header = hardy_cbor::encode::emit(&len);
                    if let Some(m) = header.first_mut() {
                        *m |= 2 << 5;
                    }
                    self.emit_inner(block_number, array, |a| {
                        a.emit_raw(header);
                        for d in data {
                            a.append_raw_slice(d);
                        }
                    })
                }
                _ => unreachable!(),
            };
            Ok::<(), hardy_cbor::decode::Error>(())
        })
        .unwrap();
    }

    pub(crate) fn write(&mut self, source_data: &[u8], array: &mut hardy_cbor::encode::Array) {
        let offset = array.offset();
        self.copy(source_data, array);
        self.data_start = offset;
    }

    pub(crate) fn copy(&self, source_data: &[u8], array: &mut hardy_cbor::encode::Array) {
        array.emit_raw_slice(&source_data[self.data_start..self.data_start + self.data_len]);
    }
}

#[derive(Clone)]
pub(crate) struct BlockWithNumber {
    pub number: u64,
    pub block: Block,
}

impl hardy_cbor::decode::FromCbor for BlockWithNumber {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse_array(data, |block, mut shortest, tags| {
            shortest = shortest && tags.is_empty() && block.is_definite();

            let block_type = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("block type code")?;

            let block_number = block.parse().map_field_err("block number").map(|(v, s)| {
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

            let flags = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("block processing control flags")?;

            let crc_type = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("CRC type")?;

            // Stash start of data
            let payload_offset = block.offset();
            block.parse_value(|value, s, tags| {
                shortest = shortest && s;
                if shortest {
                    // Appendix B of RFC9171
                    let mut seen_24 = false;
                    for tag in &tags {
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
                    hardy_cbor::decode::Value::Bytes(_) => Ok(()),
                    hardy_cbor::decode::Value::ByteStream(_) => {
                        shortest = false;
                        Ok(())
                    }
                    value => Err(hardy_cbor::decode::Error::IncorrectType(
                        "Byte String".to_string(),
                        value.type_name(!tags.is_empty()),
                    )),
                }
            })?;
            let payload_len = block.offset() - payload_offset;

            // Check CRC
            shortest = crc::parse_crc_value(data, block, crc_type)? && shortest;

            Ok((
                BlockWithNumber {
                    number: block_number,
                    block: Block {
                        block_type,
                        flags,
                        crc_type,
                        data_start: 0,
                        data_len: 0,
                        payload_offset,
                        payload_len,
                        bcb: None,
                    },
                },
                shortest,
            ))
        })
        .map(|o| {
            o.map(|((mut block, shortest), len)| {
                block.block.data_len = len;
                (block, shortest, len)
            })
        })
    }
}
