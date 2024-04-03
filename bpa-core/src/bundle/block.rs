use super::*;
use crc::Crc;
use std::collections::HashMap;

pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub data_offset: Option<usize>,
}

pub fn parse_extension_blocks(
    data: &[u8],
    mut blocks: cbor::decode::Array,
) -> Result<HashMap<u64, Block>, anyhow::Error> {
    // Use an intermediate vector so we can check the payload was the last item
    let mut extension_blocks = Vec::new();
    let extension_map = loop {
        if let Some((block_number, block)) =
            blocks.try_parse_item(|value, block_start, tags| match value {
                cbor::decode::Value::Array(a) => {
                    if !tags.is_empty() {
                        log::info!("Parsing extension block with tags");
                    }
                    Ok(Some(parse_extension_block(data, a, block_start)?))
                }
                cbor::decode::Value::End(_) => Ok(None),
                _ => Err(anyhow!("Bundle extension block is not a CBOR array")),
            })?
        {
            extension_blocks.push((block_number, block));
        } else {
            // Check the last block is the payload
            let Some((block_number, payload)) = extension_blocks.last() else {
                return Err(anyhow!("Bundle has no payload block"));
            };

            if let BlockType::Payload = payload.block_type {
                if *block_number != 1 {
                    return Err(anyhow!("Bundle payload block must be block number 1"));
                }
            } else {
                return Err(anyhow!("Final block of bundle is not a payload block"));
            }

            // Compose hashmap
            let mut map = HashMap::new();
            for (block_number, block) in extension_blocks {
                if map.insert(block_number, block).is_some() {
                    return Err(anyhow!(
                        "Bundle has more than one block with block number {}",
                        block_number
                    ));
                }
            }
            break map;
        }
    };

    // Check for duplicates

    Ok(extension_map)
}

fn parse_extension_block(
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
    let (data_start, data_len) = block.try_parse_item(|value, data_start, tags| match value {
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

pub fn parse_crc_value(
    data: &[u8],
    block_start: usize,
    mut block: cbor::decode::Array,
    crc_type: u64,
) -> Result<usize, anyhow::Error> {
    // Parse CRC
    let (crc_value, crc_start) = block.try_parse_item(|value, crc_start, tags| match value {
        cbor::decode::Value::End(_) => {
            if crc_type != 0 {
                Err(anyhow!("Block is missing required CRC value"))
            } else {
                Ok((None, crc_start))
            }
        }
        cbor::decode::Value::Uint(crc) => {
            if crc_type == 0 {
                Err(anyhow!("Block has unexpected CRC value"))
            } else {
                if !tags.is_empty() {
                    log::info!("Parsing bundle block CRC value with tags");
                }
                Ok((Some(crc), crc_start))
            }
        }
        _ => Err(anyhow!("Block CRC value must be a CBOR unsigned integer")),
    })?;

    // Confirm we are at the end of the block
    let (crc_end, block_end) = block.try_parse_item(|value, start, _| match value {
        cbor::decode::Value::End(end) => Ok((start, end)),
        _ => Err(anyhow!("Block has additional items after CRC value")),
    })?;

    // Now check CRC
    if let Some(crc_value) = crc_value {
        let err = anyhow!("Block CRC check failed");

        if crc_type == 1 {
            const X25: Crc<u16> = Crc::<u16>::new(&crc::CRC_16_IBM_SDLC);
            let mut digest = X25.digest();
            digest.update(&data[block_start..crc_start]);
            digest.update(&vec![0; crc_end - crc_start]);
            if block_end > crc_end {
                digest.update(&data[crc_end..block_end]);
            }
            if crc_value != digest.finalize() as u64 {
                return Err(err);
            }
        } else if crc_type == 2 {
            pub const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&crc::CRC_32_ISCSI);
            let mut digest = CASTAGNOLI.digest();
            digest.update(&data[block_start..crc_start]);
            digest.update(&vec![0; crc_end - crc_start]);
            if block_end > crc_end {
                digest.update(&data[crc_end..block_end]);
            }
            if crc_value != digest.finalize() as u64 {
                return Err(err);
            }
        } else {
            return Err(anyhow!("Block has invalid CRC type {}", crc_type));
        }
    }
    Ok(crc_start)
}
