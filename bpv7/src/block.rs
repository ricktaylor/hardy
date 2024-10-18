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
    pub is_bpsec_target: bool,
}

impl Block {
    pub fn block_data(&self, data: &[u8]) -> Vec<u8> {
        cbor::decode::parse(
            &data[(self.data_start + self.payload_offset)..(self.data_start + self.data_len)],
        )
        .expect("Failed to parse block data")
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
                    payload_offset += a.emit::<u64>(self.block_type.into());
                    // Block Number
                    payload_offset += a.emit(block_number);
                    // Flags
                    payload_offset += a.emit::<u64>(self.flags.into());
                    // CRC Type
                    payload_offset += a.emit::<u64>(self.crc_type.into());
                    // Payload
                    a.emit_raw(data);
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

            let (block_type, s) = block
                .parse::<(u64, bool)>()
                .map(|(v, s)| (v.into(), s))
                .map_field_err("Block type code")?;
            shortest = shortest && s;

            let (block_number, s) = block.parse().map_field_err("Block number")?;
            match block_number {
                0 => return Err(BundleError::InvalidBlockNumber),
                1 => {
                    if block_type != BlockType::Payload {
                        return Err(BundleError::InvalidBlockNumber);
                    }
                }
                _ => {
                    if block_type == BlockType::Payload {
                        return Err(BundleError::InvalidPayloadBlockNumber);
                    }
                }
            }
            shortest = shortest && s;

            let (flags, s) = block
                .parse::<(u64, bool)>()
                .map(|(v, s)| (v.into(), s))
                .map_field_err("Block processing control flags")?;
            shortest = shortest && s;

            let (crc_type, s) = block.parse().map_field_err("CRC type")?;
            shortest = shortest && s;

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
                        is_bpsec_target: false,
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
