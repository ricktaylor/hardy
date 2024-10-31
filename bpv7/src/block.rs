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
    pub fn block_data(&self, data: &[u8]) -> Result<Box<[u8]>, cbor::decode::Error> {
        cbor::decode::parse(
            &data[(self.data_start + self.payload_offset)..(self.data_start + self.data_len)],
        )
    }

    pub fn parse_payload<T>(&self, data: &[u8]) -> Result<(T, bool), BundleError>
    where
        T: cbor::decode::FromCbor<Error: From<cbor::decode::Error>>,
        BundleError: From<<T as cbor::decode::FromCbor>::Error>,
    {
        let data = self.block_data(data)?;
        let (v, s, len) = cbor::decode::parse(&data)?;
        if len != data.len() {
            Err(BundleError::BlockAdditionalData(self.block_type))
        } else {
            Ok((v, s))
        }
    }

    pub fn emit(&mut self, block_number: u64, data: &[u8], data_start: usize) -> Vec<u8> {
        let mut payload_offset = 0;
        let block_data = crc::append_crc_value(
            self.crc_type,
            cbor::encode::emit_array(
                Some(if let CrcType::None = self.crc_type {
                    5
                } else {
                    6
                }),
                |a, offset| {
                    payload_offset = offset;

                    // Block Type
                    payload_offset += a.emit(self.block_type);
                    // Block Number
                    payload_offset += a.emit(block_number);
                    // Flags
                    payload_offset += a.emit(&self.flags);
                    // CRC Type
                    payload_offset += a.emit(self.crc_type);
                    // Payload
                    a.emit(data);
                    // CRC
                    if let CrcType::None = self.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        );

        self.data_start = data_start;
        self.payload_offset = payload_offset;
        self.data_len = block_data.len();
        block_data
    }

    pub fn copy(&mut self, source_data: &[u8], data: &mut Vec<u8>) {
        let data_start = data.len();
        data.extend(&source_data[self.data_start..self.data_start + self.data_len]);
        self.data_start = data_start;
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
                .map_field_err("Block type code")?;

            let block_number = block.parse().map_field_err("Block number").map(|(v, s)| {
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
                .map_field_err("Block processing control flags")?;

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
                    cbor::decode::Value::Bytes(_, chunked) => {
                        shortest = shortest && !chunked;
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
            o.map(|((mut block, s), len)| {
                block.block.data_len = len;
                (block, s, len)
            })
        })
    }
}
