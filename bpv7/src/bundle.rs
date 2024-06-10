use super::*;
use block::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BundleError {
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
    DuplicateBlocks(BlockType),

    #[error("{0:?} block has additional data")]
    BlockAdditionalData(BlockType),

    #[error("Bundle source has no clock, and there is no Bundle Age extension block")]
    MissingBundleAge,

    #[error(transparent)]
    InvalidCrc(#[from] crc::CrcError),

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("Expecting CBOR array")]
    ArrayExpected(#[from] cbor::decode::Error),
}

pub trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, BundleError>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, BundleError> {
        self.map_err(|e| BundleError::InvalidField {
            field,
            source: e.into(),
        })
    }
}

#[derive(Default, Debug)]
pub struct Bundle {
    // From Primary Block
    pub id: BundleId,
    pub flags: BundleFlags,
    pub crc_type: CrcType,
    pub destination: Eid,
    pub report_to: Eid,
    pub lifetime: u64,

    // Unpacked from extension blocks
    pub previous_node: Option<Eid>,
    pub age: Option<u64>,
    pub hop_count: Option<HopInfo>,

    // The extension blocks
    pub blocks: std::collections::HashMap<u64, Block>,
}

#[derive(Debug, Copy, Clone)]
pub struct HopInfo {
    pub count: u64,
    pub limit: u64,
}

// For parsing a bundle plus 'minimal viability'
pub enum ValidBundle {
    Valid(Bundle),
    Invalid(Bundle),
}

fn parse_primary_block(
    data: &[u8],
    block: &mut cbor::decode::Array,
    block_start: usize,
) -> Result<(Bundle, bool), BundleError> {
    // Check number of items in the array
    if block.count().is_none() {
        trace!("Parsing primary block of indefinite length")
    }

    // Check version
    let version = block.parse::<u64>().map_field_err("Version")?;
    if version != 7 {
        return Err(BundleError::UnsupportedVersion(version));
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
    let fragment_info: Result<Option<FragmentInfo>, BundleError> = if !flags.is_fragment {
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
            crc::parse_crc_value(&data[block_start..], block, crc_type),
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

fn parse_previous_node(block: &Block, data: &[u8]) -> Result<Eid, BundleError> {
    cbor::decode::parse_detail(data)
        .map_field_err("Previous Node ID")
        .map(|(v, end, tags)| {
            if !tags.is_empty() {
                trace!("Parsing Previous Node extension block with tags");
            }
            if end != block.data_len {
                Err(BundleError::BlockAdditionalData(BlockType::PreviousNode))
            } else {
                Ok(v)
            }
        })?
}

fn parse_bundle_age(block: &Block, data: &[u8]) -> Result<u64, BundleError> {
    cbor::decode::parse_detail(data)
        .map_field_err("Bundle Age")
        .map(|(v, end, tags)| {
            if !tags.is_empty() {
                trace!("Parsing Bundle Age extension block with tags");
            }
            if end != block.data_len {
                Err(BundleError::BlockAdditionalData(BlockType::BundleAge))
            } else {
                Ok(v)
            }
        })?
}

fn parse_hop_count(block: &Block, data: &[u8]) -> Result<HopInfo, BundleError> {
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
            return Err(BundleError::BlockAdditionalData(BlockType::HopCount));
        };
        if end != block.data_len {
            return Err(BundleError::BlockAdditionalData(BlockType::HopCount));
        }
        Ok(hop_info)
    })
    .map(|(v, _)| v)
    .map_field_err("Hop Count Block")
}

impl Bundle {
    fn parse_blocks(
        &mut self,
        blocks: &mut cbor::decode::Array,
        data: &[u8],
    ) -> Result<(), BundleError> {
        // Use an intermediate vector so we can check the payload was the last item
        let mut extension_blocks = Vec::new();
        while let Some(block) = blocks.try_parse::<BlockWithNumber>()? {
            extension_blocks.push(block);
        }

        // Check the last block is the payload
        match extension_blocks.last() {
            Some(block) if block.block.block_type != BlockType::Payload => {
                return Err(BundleError::PayloadNotFinal)
            }
            Some(block) if block.number != 1 => return Err(BundleError::InvalidPayloadBlockNumber),
            Some(_) => {}
            None => return Err(BundleError::MissingPayload),
        }

        // Add blocks
        for BlockWithNumber { number, block } in extension_blocks {
            if self.blocks.insert(number, block).is_some() {
                return Err(BundleError::DuplicateBlockNumber(number));
            }
        }

        // Check the blocks
        self.parse_extension_blocks(data)
    }

    pub fn parse_extension_blocks(&mut self, data: &[u8]) -> Result<(), BundleError> {
        // Check for RFC9171-specified extension blocks
        let mut seen_payload = false;
        let mut seen_previous_node = false;
        let mut seen_bundle_age = false;
        let mut seen_hop_count = false;

        let mut previous_node = None;
        let mut bundle_age = None;
        let mut hop_count = None;

        for (block_number, block) in &self.blocks {
            let block_data = &data[block.data_offset..block.data_offset + block.data_len];
            match &block.block_type {
                BlockType::Payload => {
                    if seen_payload {
                        return Err(BundleError::DuplicateBlocks(block.block_type));
                    }
                    if *block_number != 1 {
                        return Err(BundleError::InvalidPayloadBlockNumber);
                    }
                    seen_payload = true;
                }
                BlockType::PreviousNode => {
                    if seen_previous_node {
                        return Err(BundleError::DuplicateBlocks(block.block_type));
                    }
                    previous_node = Some(parse_previous_node(block, block_data)?);
                    seen_previous_node = true;
                }
                BlockType::BundleAge => {
                    if seen_bundle_age {
                        return Err(BundleError::DuplicateBlocks(block.block_type));
                    }
                    bundle_age = Some(parse_bundle_age(block, block_data)?);
                    seen_bundle_age = true;
                }
                BlockType::HopCount => {
                    if seen_hop_count {
                        return Err(BundleError::DuplicateBlocks(block.block_type));
                    }
                    hop_count = Some(parse_hop_count(block, block_data)?);
                    seen_hop_count = true;
                }
                _ => {}
            }
        }

        if !seen_bundle_age && self.id.timestamp.creation_time.is_none() {
            return Err(BundleError::MissingBundleAge);
        }

        // Update bundle
        self.previous_node = previous_node;
        self.age = bundle_age;
        self.hop_count = hop_count;
        Ok(())
    }
}

impl cbor::decode::FromCbor for ValidBundle {
    type Error = BundleError;

    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), Self::Error> {
        let ((bundle, valid, tags), len) = cbor::decode::parse_array(data, |blocks, tags| {
            // Parse Primary block
            let (((mut bundle, mut valid), block_start), block_len) = blocks
                .parse_array(|block, block_start, tags| {
                    if !tags.is_empty() {
                        trace!("Parsing primary block with tags");
                    }
                    parse_primary_block(data, block, block_start).map(|r| (r, block_start))
                })
                .map_field_err("Primary Block")?;

            if valid {
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

                // Don't return an Err, just capture validity
                valid = bundle.parse_blocks(blocks, data).is_ok()
            }

            Ok::<_, BundleError>((bundle, valid, tags.to_vec()))
        })?;

        if valid {
            if len < data.len() {
                Err(BundleError::AdditionalData)
            } else {
                Ok((ValidBundle::Valid(bundle), len, tags.to_vec()))
            }
        } else {
            Ok((ValidBundle::Invalid(bundle), len, tags.to_vec()))
        }
    }
}
