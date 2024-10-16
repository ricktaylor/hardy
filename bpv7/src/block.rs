use super::*;
use bundle::CaptureFieldErr;

#[derive(Debug, Clone)]
pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub crc_type: CrcType,
    pub data_offset: usize,
    pub data_len: usize,
}

impl Block {
    pub fn block_data(&self, data: &[u8]) -> Vec<u8> {
        cbor::decode::parse_array(
            &data[self.data_offset..self.data_offset + self.data_len],
            |a, _, _| {
                // Block Type
                _ = a.parse::<u64>()?;
                // Block Number
                _ = a.parse::<u64>()?;
                // Flags
                _ = a.parse::<u64>()?;
                // CRC Type
                _ = a.parse::<u64>()?;
                // Payload
                let payload = a.parse();

                // Skip the crc
                a.skip_to_end(0)?;

                payload
            },
        )
        .map(|(v, _)| v)
        .expect("Failed to parse block data")
    }

    pub fn emit(&self, block_number: u64, data: &[u8]) -> Vec<u8> {
        crc::append_crc_value(
            self.crc_type,
            cbor::encode::emit_array(
                Some(if let CrcType::None = self.crc_type {
                    5
                } else {
                    6
                }),
                |a, _| {
                    // Block Type
                    a.emit::<u64>(self.block_type.into());
                    // Block Number
                    a.emit(block_number);
                    // Flags
                    a.emit::<u64>(self.flags.into());
                    // CRC Type
                    a.emit::<u64>(self.crc_type.into());
                    // Payload
                    a.emit_raw(data);
                    // CRC
                    if let CrcType::None = self.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        )
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

            let (block_type, s, _) = block
                .parse::<(u64, bool, usize)>()
                .map(|(v, s, l)| (v.into(), s, l))
                .map_field_err("Block type code")?;
            shortest = shortest && s;

            let (block_number, s, _) = block
                .parse::<(u64, bool, usize)>()
                .map_field_err("Block number")?;
            shortest = shortest && s;
            if block_number == 0 {
                return Err(BundleError::InvalidBlockNumber);
            }

            let (flags, s, _) = block
                .parse::<(u64, bool, usize)>()
                .map(|(v, s, l)| (v.into(), s, l))
                .map_field_err("Block processing control flags")?;
            shortest = shortest && s;

            let (crc_type, s, _) = block
                .parse::<(CrcType, bool, usize)>()
                .map_field_err("CRC type")?;
            shortest = shortest && s;

            // Stash start of data
            block.parse_value(|value, _, s, tags| {
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
                        data_offset: 0,
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
