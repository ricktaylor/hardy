use super::*;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Bundle has additional data after end of CBOR array")]
    AdditionalData,

    #[error("Unsupported bundle protocol version {0}")]
    UnsupportedVersion(u64),

    #[error("Bundle has no payload block")]
    MissingPayload,

    #[error("Bundle payload block must be block number 1")]
    InvalidPayloadBlockNumber,

    #[error("Final block of bundle is not a payload block")]
    PayloadNotFinal,

    #[error("Bundle has more than one block with block number {0}")]
    DuplicateBlockNumber(u64),

    #[error("Bundle block must not be block number 0")]
    InvalidBlockNumber,

    #[error("Bundle has multiple {0:?} blocks")]
    DuplicateBlocks(bundle::BlockType),

    #[error("{0:?} block has additional data")]
    BlockAdditionalData(bundle::BlockType),

    #[error("Bundle source has no clock, and there is no Bundle Age extension block")]
    MissingBundleAge,

    #[error(transparent)]
    InvalidCrc(#[from] crc::Error),

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("Expecting CBOR array")]
    ArrayExpected(#[from] cbor::decode::Error),
}

trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, Error>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, Error> {
        self.map_err(|e| Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}

pub fn parse(data: &[u8]) -> Result<(Bundle, bool), Error> {
    let ((bundle, valid), len) = cbor::decode::parse_array(data, |blocks, tags| {
        if !tags.is_empty() {
            trace!("Parsing bundle with tags");
        }
        parse_blocks(data, blocks)
    })?;
    if valid && len < data.len() {
        return Err(Error::AdditionalData);
    }
    Ok((bundle, valid))
}

fn parse_blocks(data: &[u8], blocks: &mut cbor::decode::Array) -> Result<(Bundle, bool), Error> {
    // Parse Primary block
    let (mut bundle, valid, block_start, block_len) = blocks
        .parse_array(|a, block_start, tags| {
            if !tags.is_empty() {
                trace!("Parsing primary block with tags");
            }
            parse_primary_block(data, a, block_start)
                .map(|(bundle, valid)| (bundle, valid, block_start))
        })
        .map(|((bundle, valid, block_start), len)| (bundle, valid, block_start, len))
        .map_field_err("Primary Block")?;

    // Add a block 0
    bundle.blocks.insert(
        0,
        Block {
            block_type: BlockType::Primary,
            flags: BlockFlags {
                report_on_failure: true,
                delete_bundle_on_failure: true,
                ..Default::default()
            },
            crc_type: bundle.crc_type,
            data_offset: block_start,
            data_len: block_len,
        },
    );

    let valid = if valid {
        // Don't return an Err, we need to return Ok(invalid)
        parse_extension_blocks(data, blocks)
            .map(|bundle_blocks| {
                bundle.blocks = bundle_blocks;
                check_blocks(&mut bundle, data).is_ok()
            })
            .is_ok()
    } else {
        false
    };

    Ok((bundle, valid))
}

fn parse_primary_block(
    data: &[u8],
    block: &mut cbor::decode::Array,
    block_start: usize,
) -> Result<(Bundle, bool), Error> {
    // Check number of items in the array
    if block.count().is_none() {
        trace!("Parsing primary block of indefinite length")
    }

    // Check version
    let version = block.parse::<u64>().map_field_err("Version")?;
    if version != 7 {
        return Err(Error::UnsupportedVersion(version));
    }

    // Parse flags
    let flags: BundleFlags = block
        .parse::<u64>()
        .map_field_err("Bundle Processing Control Flags")?
        .into();

    // Parse CRC Type
    let crc_type = block.parse::<CrcType>().map_field_err("CRC Type");

    // Parse EIDs
    let dest_eid = block.parse::<Eid>().map_field_err("Destination EID");
    let source_eid = block.parse::<Eid>().map_field_err("Source EID");
    let report_to_eid = block.parse::<Eid>().map_field_err("Report-to EID")?;

    // Parse timestamp
    let timestamp = block
        .parse::<CreationTimestamp>()
        .map_field_err("Creation Timestamp");

    // Parse lifetime
    let lifetime = block.parse::<u64>().map_field_err("Lifetime");

    // Parse fragment parts
    let fragment_info: Result<Option<FragmentInfo>, Error> = if !flags.is_fragment {
        Ok(None)
    } else {
        Ok(Some(FragmentInfo {
            offset: block.parse().map_field_err("Fragment offset")?,
            total_len: block
                .parse()
                .map_field_err("Total Application Data Unit Length")?,
        }))
    };

    // Try to parse and check CRC
    let crc_result = crc_type.map(|crc_type| {
        (
            crc::parse_crc_value(data, block_start, block, crc_type),
            crc_type,
        )
    });

    // By the time we get here we have just enough information to react to an invalid primary block
    match (
        dest_eid,
        source_eid,
        timestamp,
        lifetime,
        fragment_info,
        crc_result,
    ) {
        (
            Ok(destination),
            Ok(source),
            Ok(timestamp),
            Ok(lifetime),
            Ok(fragment_info),
            Ok((Ok(_), crc_type)),
        ) => Ok((
            Bundle {
                id: BundleId {
                    source,
                    timestamp,
                    fragment_info,
                },
                flags,
                crc_type,
                destination,
                report_to: report_to_eid,
                lifetime,
                ..Default::default()
            },
            true,
        )),
        (dest_eid, source_eid, timestamp, lifetime, _, crc_result) => {
            Ok((
                // Compose something out of what we have!
                Bundle {
                    id: BundleId {
                        source: source_eid.unwrap_or(Eid::Null),
                        timestamp: timestamp.unwrap_or(CreationTimestamp::default()),
                        ..Default::default()
                    },
                    flags,
                    crc_type: crc_result.map_or(CrcType::None, |(_, t)| t),
                    destination: dest_eid.unwrap_or(Eid::Null),
                    report_to: report_to_eid,
                    lifetime: lifetime.unwrap_or(0),
                    ..Default::default()
                },
                false,
            ))
        }
    }
}

fn parse_extension_blocks(
    data: &[u8],
    blocks: &mut cbor::decode::Array,
) -> Result<HashMap<u64, Block>, Error> {
    // Use an intermediate vector so we can check the payload was the last item
    let mut extension_blocks = Vec::new();
    let extension_map = loop {
        if let Some(((block_number, block), _)) =
            blocks.try_parse_array(|a, block_start, tags| {
                if !tags.is_empty() {
                    trace!("Parsing extension block with tags");
                }
                parse_block(data, a, block_start)
            })?
        {
            extension_blocks.push((block_number, block));
        } else {
            // Check the last block is the payload
            let Some((block_number, payload)) = extension_blocks.last() else {
                return Err(Error::MissingPayload);
            };

            if let BlockType::Payload = payload.block_type {
                if *block_number != 1 {
                    return Err(Error::InvalidPayloadBlockNumber);
                }
            } else {
                return Err(Error::PayloadNotFinal);
            }

            // Compose hashmap
            let mut map = HashMap::new();
            for (block_number, block) in extension_blocks {
                if map.insert(block_number, block).is_some() {
                    return Err(Error::DuplicateBlockNumber(block_number));
                }
            }
            break map;
        }
    };
    Ok(extension_map)
}

fn parse_block(
    data: &[u8],
    block: &mut cbor::decode::Array,
    block_start: usize,
) -> Result<(u64, Block), Error> {
    // Check number of items in the array
    if block.count().is_none() {
        trace!("Parsing extension block of indefinite length")
    }

    let block_type = block
        .parse::<u64>()
        .map_field_err("Block type code")?
        .into();

    let block_number = block.parse::<u64>().map_field_err("Block number")?;
    if block_number == 0 {
        return Err(Error::InvalidBlockNumber);
    }

    let flags = block
        .parse::<u64>()
        .map_field_err("Block processing control flags")?
        .into();
    let crc_type = block.parse::<CrcType>().map_field_err("CRC type")?;

    // Stash start of data
    let ((data_offset, _), data_len) =
        block.parse_value(|value, data_start, tags| match value {
            cbor::decode::Value::Bytes(v, chunked) => {
                if chunked {
                    trace!("Parsing chunked extension block data");
                }
                if !tags.is_empty() {
                    trace!("Parsing extension block data with tags");
                }
                Ok((data_start, v.len()))
            }
            value => Err(cbor::decode::Error::IncorrectType(
                "Byte String".to_string(),
                value.type_name(),
            )),
        })?;

    // Check CRC
    crc::parse_crc_value(data, block_start, block, crc_type)?;

    Ok((
        block_number,
        Block {
            block_type,
            flags,
            crc_type,
            data_offset,
            data_len,
        },
    ))
}

pub fn check_blocks(bundle: &mut Bundle, data: &[u8]) -> Result<(), Error> {
    // Check for RFC9171-specified extension blocks
    let mut seen_payload = false;
    let mut seen_previous_node = false;
    let mut seen_bundle_age = false;
    let mut seen_hop_count = false;

    let mut previous_node = None;
    let mut bundle_age = None;
    let mut hop_count = None;

    for (block_number, block) in &bundle.blocks {
        let block_data = &data[block.data_offset..block.data_offset + block.data_len];
        match &block.block_type {
            BlockType::Payload => {
                if seen_payload {
                    return Err(Error::DuplicateBlocks(block.block_type));
                }
                if *block_number != 1 {
                    return Err(Error::InvalidPayloadBlockNumber);
                }
                seen_payload = true;
            }
            BlockType::PreviousNode => {
                if seen_previous_node {
                    return Err(Error::DuplicateBlocks(block.block_type));
                }
                previous_node = Some(check_previous_node(block, block_data)?);
                seen_previous_node = true;
            }
            BlockType::BundleAge => {
                if seen_bundle_age {
                    return Err(Error::DuplicateBlocks(block.block_type));
                }
                bundle_age = Some(check_bundle_age(block, block_data)?);
                seen_bundle_age = true;
            }
            BlockType::HopCount => {
                if seen_hop_count {
                    return Err(Error::DuplicateBlocks(block.block_type));
                }
                hop_count = Some(check_hop_count(block, block_data)?);
                seen_hop_count = true;
            }
            _ => {}
        }
    }

    if !seen_bundle_age && bundle.id.timestamp.creation_time == 0 {
        return Err(Error::MissingBundleAge);
    }

    // Update bundle
    bundle.previous_node = previous_node;
    bundle.age = bundle_age;
    bundle.hop_count = hop_count;
    Ok(())
}

fn check_previous_node(block: &Block, data: &[u8]) -> Result<Eid, Error> {
    cbor::decode::parse_detail(data)
        .map_field_err("Previous Node ID")
        .map(|(v, end, tags)| {
            if !tags.is_empty() {
                trace!("Parsing Previous Node extension block with tags");
            }
            if end != block.data_len {
                Err(Error::BlockAdditionalData(BlockType::PreviousNode))
            } else {
                Ok(v)
            }
        })?
}

fn check_bundle_age(block: &Block, data: &[u8]) -> Result<u64, Error> {
    cbor::decode::parse_detail(data)
        .map_field_err("Bundle Age")
        .map(|(v, end, tags)| {
            if !tags.is_empty() {
                trace!("Parsing Bundle Age extension block with tags");
            }
            if end != block.data_len {
                Err(Error::BlockAdditionalData(BlockType::BundleAge))
            } else {
                Ok(v)
            }
        })?
}

fn check_hop_count(block: &Block, data: &[u8]) -> Result<HopInfo, Error> {
    cbor::decode::parse_array(data, |a, tags| {
        if !tags.is_empty() {
            trace!("Parsing Hop Count with tags");
        }
        if a.count().is_none() {
            trace!("Parsing Hop Count as indefinite length array");
        }

        let hop_info = HopInfo {
            limit: a.parse().map_field_err("hop limit")?,
            count: a.parse().map_field_err("hop count")?,
        };

        let Some(end) = a.end()? else {
            return Err(Error::BlockAdditionalData(BlockType::HopCount));
        };
        if end != block.data_len {
            return Err(Error::BlockAdditionalData(BlockType::HopCount));
        }
        Ok(hop_info)
    })
    .map(|(v, _)| v)
    .map_field_err("Hop Count Block")
}
