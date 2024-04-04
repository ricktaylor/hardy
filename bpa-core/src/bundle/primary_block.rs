use super::*;

pub struct FragmentInfo {
    pub offset: u64,
    pub total_len: u64,
}

pub struct PrimaryBlock {
    pub flags: BundleFlags,
    pub source: Eid,
    pub destination: Eid,
    pub report_to: Eid,
    pub timestamp: (u64, u64),
    pub lifetime: u64,
    pub fragment_info: Option<FragmentInfo>,
}

pub fn parse(
    data: &[u8],
    mut block: cbor::decode::Array,
    block_start: usize,
) -> Result<(PrimaryBlock, bool), anyhow::Error> {
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
        .parse::<u64>()
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
    let timestamp = parse_timestamp(&mut block);

    // Parse lifetime
    let lifetime = block.parse::<u64>();

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
        Ok(crc_type) => block::parse_crc_value(data, block_start, block, crc_type),
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
        (Ok(destination), Ok(source), Ok(timestamp), Ok(lifetime), Ok(fragment_info), Ok(_)) => {
            Ok((
                PrimaryBlock {
                    flags,
                    source,
                    destination,
                    report_to: report_to_eid,
                    timestamp,
                    lifetime,
                    fragment_info,
                },
                true,
            ))
        }
        (dest_eid, source_eid, timestamp, lifetime, _, _) => {
            Ok((
                // Compose something out of what we have!
                PrimaryBlock {
                    flags,
                    source: source_eid.unwrap_or(Eid::Null),
                    destination: dest_eid.unwrap_or(Eid::Null),
                    report_to: report_to_eid,
                    timestamp: timestamp.unwrap_or((0, 0)),
                    lifetime: lifetime.unwrap_or(0),
                    fragment_info: None,
                },
                false,
            ))
        }
    }
}

fn parse_timestamp(block: &mut cbor::decode::Array) -> Result<(u64, u64), anyhow::Error> {
    block.try_parse_item(|value, _, tags| {
        if let cbor::decode::Value::Array(mut a) = value {
            if !tags.is_empty() {
                log::info!("Parsing bundle primary block timestamp with tags");
            }

            let creation_time = a.parse::<u64>()?;
            let seq_no = a.parse::<u64>()?;
            Ok((creation_time, seq_no))
        } else {
            Err(anyhow!(
                "Bundle primary block timestamp must be a CBOR array"
            ))
        }
    })
}