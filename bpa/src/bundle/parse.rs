use super::*;

pub fn parse_bundle(data: &[u8]) -> Result<(Bundle, bool), anyhow::Error> {
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

fn parse_bundle_blocks(
    data: &[u8],
    mut blocks: cbor::decode::Array,
) -> Result<(Bundle, bool), anyhow::Error> {
    // Parse Primary block
    let (mut bundle, valid) = blocks.try_parse_item(|value, block_start, tags| {
        if let cbor::decode::Value::Array(a) = value {
            if !tags.is_empty() {
                log::info!("Parsing primary block with tags");
            }
            parse_primary_block(data, a, block_start)
        } else {
            Err(anyhow!("Bundle primary block is not a CBOR array"))
        }
    })?;

    let valid = if valid {
        // Parse other blocks
        match parse_extension_blocks(data, blocks) {
            Ok(bundle_blocks) => {
                bundle.blocks = bundle_blocks;
                true
            }
            Err(e) => {
                // Don't return an Err, we need to return Ok(invalid)
                log::info!("Extension block parsing failed: {}", e);
                false
            }
        }
    } else {
        false
    };

    Ok((bundle, valid))
}

fn parse_primary_block(
    data: &[u8],
    mut block: cbor::decode::Array,
    block_start: usize,
) -> Result<(Bundle, bool), anyhow::Error> {
    // Check number of items in the array
    match block.count() {
        None => log::info!("Parsing primary block of indefinite length"),
        Some(count) if !(8..=11).contains(&count) => {
            return Err(anyhow!("Bundle primary block has {} array items", count))
        }
        _ => {}
    }

    // Check version
    let version = block.parse::<u64>()?;
    if version != 7 {
        return Err(anyhow!("Unsupported bundle protocol version {}", version));
    }

    // Parse flags
    let flags = block.parse::<BundleFlags>()?;

    // Parse CRC Type
    let crc_type = block
        .parse::<CrcType>()
        .inspect_err(|e| log::info!("Invalid crc type: {}", e));

    // Parse EIDs
    let dest_eid = block
        .parse::<Eid>()
        .inspect_err(|e| log::info!("Invalid destination EID: {}", e));
    let source_eid = block
        .parse::<Eid>()
        .inspect_err(|e| log::info!("Invalid source EID: {}", e));
    let report_to_eid = block
        .parse::<Eid>()
        .inspect_err(|e| log::info!("Invalid report-to EID: {}", e))?;

    // Parse timestamp
    let timestamp = block.parse::<CreationTimestamp>();

    // Parse lifetime
    let lifetime = block.parse::<u64>().inspect_err(|e| log::info!("Invalid lifetime: {}", e));

    // Parse fragment parts
    let fragment_info: Result<Option<FragmentInfo>, anyhow::Error> = if !flags.is_fragment {
        Ok(None)
    } else {
        let offset = block.parse::<u64>()?;
        let total_len = block.parse::<u64>()?;
        Ok(Some(FragmentInfo { offset, total_len }))
    };

    // Try to parse and check CRC
    let crc_result = match crc_type {
        Ok(crc_type) => Ok((
            crc::parse_crc_value(data, block_start, block, crc_type),
            crc_type,
        )),
        Err(e) => Err(e),
    };

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
            Ok((_, crc_type)),
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
                blocks: HashMap::new(),
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
                        fragment_info: None,
                    },
                    flags,
                    crc_type: crc_result.map_or(CrcType::None, |(_, t)| t),
                    destination: dest_eid.unwrap_or(Eid::Null),
                    report_to: report_to_eid,
                    lifetime: lifetime.unwrap_or(0),
                    blocks: HashMap::new(),
                },
                false,
            ))
        }
    }
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
                    Ok(Some(parse_block(data, a, block_start)?))
                }
                cbor::decode::Value::End(_) => Ok(None),
                _ => Err(anyhow!("Bundle extension block is not a CBOR array")),
            })?
        {
            if block_number == 0 {
                return Err(anyhow!("Bundle extension block must not be block number 0"));
            }
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

fn parse_block(
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
    let crc_type = block.parse::<CrcType>()?;

    // Stash start of data
    let (data_offset, data_len) = block.try_parse_item(|value, data_start, tags| match value {
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
