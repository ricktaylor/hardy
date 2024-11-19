use super::*;
use bundle::CaptureFieldErr;

#[derive(Debug, Clone)]
pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub crc_type: CrcType,
    pub data_start: usize,
    pub payload_offset: usize,
    pub data_len: usize,
}

impl Block {
    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.data_start + self.payload_offset..self.data_start + self.data_len]
    }

    pub fn parse_payload<T>(
        &self,
        source_data: &[u8],
    ) -> Result<(T, bool), <T as cbor::decode::FromCbor>::Error>
    where
        T: cbor::decode::FromCbor<Error: From<cbor::decode::Error>>,
    {
        let payload_data = self.payload(source_data);
        cbor::decode::parse_value(payload_data, |v, shortest, tags| match v {
            cbor::decode::Value::Bytes(data) => cbor::decode::parse::<(T, bool, usize)>(data)
                .map(|(v, s, len)| (v, shortest && s && tags.is_empty() && len == data.len())),
            cbor::decode::Value::ByteStream(data) => cbor::decode::parse::<(T, bool, usize)>(
                &data.iter().fold(Vec::new(), |mut data, d| {
                    data.extend(*d);
                    data
                }),
            )
            .map(|v| (v.0, false)),
            _ => unreachable!(),
        })
        .map(|((v, s), len)| (v, s && len == payload_data.len()))
    }

    pub fn emit(&mut self, block_number: u64, data: &[u8], array: &mut cbor::encode::Array) {
        let block_data = crc::append_crc_value(
            self.crc_type,
            cbor::encode::emit_array(
                Some(if let CrcType::None = self.crc_type {
                    5
                } else {
                    6
                }),
                |a| {
                    a.emit(self.block_type);
                    a.emit(block_number);
                    a.emit(&self.flags);
                    a.emit(self.crc_type);

                    // Payload
                    self.payload_offset = a.offset();
                    a.emit(data);

                    // CRC
                    if let CrcType::None = self.crc_type {
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

    pub fn rewrite(
        &mut self,
        block_number: u64,
        array: &mut cbor::encode::Array,
        source_data: &[u8],
    ) -> Result<(), cbor::decode::Error> {
        cbor::decode::parse_value(self.payload(source_data), |value, _, _| {
            match value {
                cbor::decode::Value::Bytes(d) => self.emit(block_number, d, array),
                cbor::decode::Value::ByteStream(d) => self.emit(
                    block_number,
                    &d.iter().fold(Vec::new(), |mut data, d| {
                        data.extend(*d);
                        data
                    }),
                    array,
                ),
                _ => unreachable!(),
            };
            Ok(())
        })
        .map(|_| ())
    }

    pub fn copy(&mut self, source_data: &[u8], array: &mut cbor::encode::Array) {
        let offset = array.offset();
        array.emit_raw_slice(&source_data[self.data_start..self.data_start + self.data_len]);
        self.data_start = offset;
    }
}

#[derive(Clone)]
pub struct BlockWithNumber {
    pub number: u64,
    pub block: Block,
}

impl cbor::decode::FromCbor for BlockWithNumber {
    type Error = BundleError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |block, mut shortest, tags| {
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
                (1, BlockType::Payload) => {}
                (0, _) | (1, _) | (_, BlockType::Primary) | (_, BlockType::Payload) => {
                    return Err(BundleError::InvalidBlockNumber(block_number, block_type))
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
                    cbor::decode::Value::Bytes(_) => Ok(()),
                    cbor::decode::Value::ByteStream(_) => {
                        shortest = false;
                        Ok(())
                    }
                    value => Err(cbor::decode::Error::IncorrectType(
                        "Byte String".to_string(),
                        value.type_name(!tags.is_empty()),
                    )),
                }
            })?;

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
                        payload_offset,
                        data_len: 0,
                    },
                },
                shortest,
            ))
        })
        .map(|o| {
            o.map(|((mut block, mut shortest), len)| {
                if let CrcType::Unrecognised(_) = &block.block.crc_type {
                    // The CRC stops us canonicalising this block
                    shortest = true;
                }
                block.block.data_len = len;
                (block, shortest, len)
            })
        })
    }
}
