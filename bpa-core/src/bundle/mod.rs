use anyhow::anyhow;
use crc::Crc;
use hardy_cbor as cbor;
use std::collections::HashMap;

mod block;
mod block_flags;
mod block_type;
mod bundle_flags;
mod bundle_status;
mod eid;
mod primary_block;

pub use block::Block;
pub use block_flags::BlockFlags;
pub use block_type::BlockType;
pub use bundle_flags::BundleFlags;
pub use bundle_status::BundleStatus;
pub use eid::Eid;
pub use primary_block::{FragmentInfo, PrimaryBlock};

pub struct Metadata {
    pub status: BundleStatus,
    pub storage_name: String,
    pub hash: String,
    pub received_at: time::OffsetDateTime,
}

pub struct Bundle {
    pub metadata: Option<Metadata>,
    pub primary: PrimaryBlock,
    pub extensions: HashMap<u64, Block>,
}

impl Bundle {
    pub fn parse(data: &[u8]) -> Result<(Self, bool), anyhow::Error> {
        let ((bundle, valid), consumed) = cbor::decode::parse_value(data, |value, tags| {
            if let cbor::decode::Value::Array(blocks) = value {
                if !tags.is_empty() {
                    log::info!("Parsing bundle with tags");
                }
                parse_bundle_blocks(data, blocks)
            } else {
                Err(anyhow!("Bundle is not a CBOR array"))
            }
        })?;
        if valid && consumed < data.len() {
            return Err(anyhow!(
                "Bundle has additional data after end of CBOR array"
            ));
        }
        Ok((bundle, valid))
    }
}

fn parse_bundle_blocks(
    data: &[u8],
    mut blocks: cbor::decode::Array,
) -> Result<(Bundle, bool), anyhow::Error> {
    // Parse Primary block
    let (primary, valid) = blocks.try_parse_item(|value, block_start, tags| {
        if let cbor::decode::Value::Array(a) = value {
            if !tags.is_empty() {
                log::info!("Parsing primary block with tags");
            }
            PrimaryBlock::parse(data, a, block_start)
        } else {
            Err(anyhow!("Bundle primary block is not a CBOR array"))
        }
    })?;

    let (extensions, valid) = if valid {
        // Parse other blocks
        match parse_extension_blocks(data, blocks) {
            Ok(extensions) => (extensions, true),
            Err(e) => {
                // Don't return an Err, we need to return Ok(invalid)
                log::info!("Extension block parsing failed: {}", e);
                (HashMap::new(), false)
            }
        }
    } else {
        (HashMap::new(), false)
    };

    Ok((
        Bundle {
            metadata: None,
            primary,
            extensions,
        },
        valid,
    ))
}

fn parse_extension_blocks(
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
                    Ok(Some(Block::parse(data, a, block_start)?))
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

fn parse_crc_value(
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

pub fn dtn_time(instant: &time::OffsetDateTime) -> u64 {
    (*instant - time::macros::datetime!(2000-01-01 00:00:00 UTC)).whole_milliseconds() as u64
}
