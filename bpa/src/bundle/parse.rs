use super::*;

pub fn parse(data: &[u8]) -> Result<(Bundle, bool), anyhow::Error> {
    let ((mut bundle, mut valid), len) = cbor::decode::parse_array(data, |blocks, tags| {
        if !tags.is_empty() {
            log::info!("Parsing bundle with tags");
        }
        parse_blocks(data, blocks)
    })?;
    if valid {
        if len < data.len() {
            return Err(anyhow!(
                "Bundle has additional data after end of CBOR array"
            ));
        }

        valid = check_blocks(&mut bundle, data)
            .inspect_err(|e| log::info!("{}", e))
            .is_ok();
    }
    Ok((bundle, valid))
}

fn parse_blocks(
    data: &[u8],
    blocks: &mut cbor::decode::Array,
) -> Result<(Bundle, bool), anyhow::Error> {
    // Parse Primary block
    let (mut bundle, valid, block_start, block_len) = blocks
        .parse_array(|a, block_start, tags| {
            if !tags.is_empty() {
                log::info!("Parsing primary block with tags");
            }
            parse_primary_block(data, a, block_start)
                .map(|(bundle, valid)| (bundle, valid, block_start))
        })
        .map(|((bundle, valid, block_start), len)| (bundle, valid, block_start, len))?;

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
    block: &mut cbor::decode::Array,
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
    let lifetime = block
        .parse::<u64>()
        .inspect_err(|e| log::info!("Invalid lifetime: {}", e));

    // Parse fragment parts
    let fragment_info: Result<Option<FragmentInfo>, anyhow::Error> = if !flags.is_fragment {
        Ok(None)
    } else {
        Ok(Some(FragmentInfo {
            offset: block
                .parse()
                .inspect_err(|e| log::info!("Invalid fragment offset: {}", e))?,
            total_len: block
                .parse()
                .inspect_err(|e| log::info!("Invalid application data total length: {}", e))?,
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
) -> Result<HashMap<u64, Block>, anyhow::Error> {
    // Use an intermediate vector so we can check the payload was the last item
    let mut extension_blocks = Vec::new();
    let extension_map = loop {
        if let Some(((block_number, block), _)) =
            blocks.try_parse_array(|a, block_start, tags| {
                if !tags.is_empty() {
                    log::info!("Parsing extension block with tags");
                }
                parse_block(data, a, block_start)
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
    Ok(extension_map)
}

fn parse_block(
    data: &[u8],
    block: &mut cbor::decode::Array,
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
    if let BlockType::Primary = block_type {
        return Err(anyhow!("Bundle extension block must not be block type 0"));
    }

    let block_number = block.parse::<u64>()?;
    if block_number == 0 {
        return Err(anyhow!("Bundle extension block must not be block number 0"));
    }

    let flags = block.parse::<BlockFlags>()?;
    let crc_type = block.parse::<CrcType>()?;

    // Stash start of data
    let ((data_offset, _), data_len) =
        block.parse_value(|value, data_start, tags| match value {
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

pub fn check_blocks(bundle: &mut Bundle, data: &[u8]) -> Result<(), anyhow::Error> {
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
                    return Err(anyhow!("Bundle has multiple payload blocks"));
                }
                if *block_number != 1 {
                    return Err(anyhow!(
                        "Bundle has payload block with number {}",
                        block_number
                    ));
                }
                seen_payload = true;
            }
            BlockType::PreviousNode => {
                if seen_previous_node {
                    return Err(anyhow!(
                        "Bundle has multiple Previous Node extension blocks"
                    ));
                }
                previous_node = Some(check_previous_node(block, block_data)?);
                seen_previous_node = true;
            }
            BlockType::BundleAge => {
                if seen_bundle_age {
                    return Err(anyhow!("Bundle has multiple Bundle Age extension blocks"));
                }

                bundle_age = Some(check_bundle_age(block, block_data)?);
                seen_bundle_age = true;
            }
            BlockType::HopCount => {
                if seen_hop_count {
                    return Err(anyhow!("Bundle has multiple Hop Count extension blocks"));
                }
                hop_count = Some(check_hop_count(block, block_data)?);
                seen_hop_count = true;
            }
            _ => {}
        }
    }

    if !seen_bundle_age && bundle.id.timestamp.creation_time == 0 {
        return Err(anyhow!(
            "Bundle source has no clock, and there is no Bundle Age extension block"
        ));
    }

    // Update bundle
    bundle.previous_node = previous_node;
    bundle.age = bundle_age;
    bundle.hop_count = hop_count;
    Ok(())
}

fn check_previous_node(block: &Block, data: &[u8]) -> Result<Eid, anyhow::Error> {
    cbor::decode::parse_detail(data)
        .map_err(|e| anyhow!("Failed to parse EID in Previous Node block: {}", e))
        .map(|(v, end, tags)| {
            if !tags.is_empty() {
                log::info!("Parsing Previous Node extension block with tags");
            }
            if end != block.data_len {
                Err(anyhow!("Previous Node extension block has additional data"))
            } else {
                Ok(v)
            }
        })?
}

fn check_bundle_age(block: &Block, data: &[u8]) -> Result<u64, anyhow::Error> {
    cbor::decode::parse_detail(data)
        .map_err(|e| anyhow!("Failed to parse age in Bundle Age block: {}", e))
        .map(|(v, end, tags)| {
            if !tags.is_empty() {
                log::info!("Parsing Bundle Age extension block with tags");
            }
            if end != block.data_len {
                Err(anyhow!("Bundle Age extension block has additional data"))
            } else {
                Ok(v)
            }
        })?
}

fn check_hop_count(block: &Block, data: &[u8]) -> Result<HopInfo, anyhow::Error> {
    cbor::decode::parse_array(data, |a, tags| {
        if !tags.is_empty() {
            log::info!("Parsing Hop Count with tags");
        }
        if a.count().is_none() {
            log::info!("Parsing Hop Count as indefinite length array");
        }

        let hop_info = HopInfo {
            count: a.parse()?,
            limit: a.parse()?,
        };

        let end = a.end_or_else(|| anyhow!("Hop Count extension block has too many items"))?;
        if end != block.data_len {
            return Err(anyhow!("Hop Count extension block has additional data"));
        }
        Ok(hop_info)
    })
    .map(|(v, _)| v)
}
