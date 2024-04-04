use super::*;

pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub data_offset: Option<usize>,
}

impl Block {
    pub fn parse(
        data: &[u8],
        mut block: cbor::decode::Array,
        block_start: usize,
    ) -> Result<(u64, Block), anyhow::Error> {
        // Check number of items in the array
        match block.count() {
            None => log::info!("Parsing extension block of indefinite length"),
            Some(count) if !(5..=6).contains(&count) => {
                return Err(anyhow!("Extension block has {} elements", count))
            }
            _ => {}
        }

        let block_type = block.parse::<BlockType>()?;
        let block_number = block.parse::<u64>()?;
        let flags = block.parse::<BlockFlags>()?;
        let crc_type = block.parse::<u64>()?;

        // Stash start of data
        let (data_start, data_len) =
            block.try_parse_item(|value, data_start, tags| match value {
                cbor::decode::Value::Bytes(v, chunked) => {
                    if chunked {
                        log::info!("Parsing chunked extension block data");
                    }
                    if !tags.is_empty() {
                        log::info!("Parsing extension block data with tags");
                    }
                    Ok((data_start, v.len()))
                }
                _ => Err(anyhow!("Block data must be encoded as a CBOR byte string")),
            })?;

        // Check CRC
        let data_end = parse_crc_value(data, block_start, block, crc_type)?;

        Ok((
            block_number,
            Block {
                block_type,
                flags,
                data_offset: if data_end == data_start || data_len == 0 {
                    None
                } else {
                    Some(data_start)
                },
            },
        ))
    }
}
