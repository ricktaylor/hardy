use super::*;
use error::CaptureFieldErr;

#[derive(Debug, Clone)]
pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub crc_type: CrcType,
    pub data_start: usize,
    pub data_len: usize,
    pub payload_offset: usize,
    pub payload_len: usize,
    pub bcb: Option<u64>,
}

impl Block {
    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.data_start + self.payload_offset
            ..self.data_start + self.payload_offset + self.payload_len]
    }

    fn emit_inner(
        &mut self,
        block_number: u64,
        array: &mut cbor::encode::Array,
        f: impl FnOnce(&mut cbor::encode::Array),
    ) {
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
                    f(a);

                    // CRC
                    if let CrcType::None = self.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        );
        self.data_start = array.offset();
        self.payload_len = block_data.len();
        array.emit_raw(block_data)
    }

    pub(crate) fn emit(&mut self, block_number: u64, data: &[u8], array: &mut cbor::encode::Array) {
        self.emit_inner(block_number, array, |a| a.emit(data));
    }

    pub(crate) fn rewrite(
        &mut self,
        block_number: u64,
        array: &mut cbor::encode::Array,
        source_data: &[u8],
    ) {
        cbor::decode::parse_value(self.payload(source_data), |value, _, _| {
            match value {
                cbor::decode::Value::Bytes(data) => self.emit(block_number, data, array),
                cbor::decode::Value::ByteStream(data) => {
                    // This is horrible, but removes a potentially large data copy
                    let len = data.iter().fold(0u64, |len, d| len + d.len() as u64);
                    let mut header = cbor::encode::emit(len);
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
            Ok::<(), cbor::decode::Error>(())
        })
        .unwrap();
    }

    pub(crate) fn write(&mut self, source_data: &[u8], array: &mut cbor::encode::Array) {
        let offset = array.offset();
        self.copy(source_data, array);
        self.data_start = offset;
    }

    pub(crate) fn copy(&self, source_data: &[u8], array: &mut cbor::encode::Array) {
        array.emit_raw_slice(&source_data[self.data_start..self.data_start + self.data_len]);
    }
}

#[derive(Clone)]
pub struct BlockWithNumber {
    pub number: u64,
    pub block: Block,
}

impl cbor::decode::FromCbor for BlockWithNumber {
    type Error = Error;

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
