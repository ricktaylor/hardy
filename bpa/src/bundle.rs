use std::collections::HashMap;

use super::*;
use anyhow::anyhow;
use crc::Crc;

#[derive(Default)]
pub struct BundleFlags {
    pub is_fragment: bool,
    pub is_admin_record: bool,
    pub do_not_fragment: bool,
    pub app_ack_requested: bool,
    pub status_time_requested: bool,
    pub receipt_report_requested: bool,
    pub forward_report_requested: bool,
    pub delivery_report_requested: bool,
    pub delete_report_requested: bool,
}

impl BundleFlags {
    pub fn new(f: u64) -> Self {
        let mut flags = BundleFlags::default();
        for b in 0..=20 {
            if f & (1 << b) != 0 {
                match b {
                    0 => flags.is_fragment = true,
                    1 => flags.is_admin_record = true,
                    2 => flags.do_not_fragment = true,
                    5 => flags.app_ack_requested = true,
                    6 => flags.status_time_requested = true,
                    14 => flags.receipt_report_requested = true,
                    16 => flags.forward_report_requested = true,
                    17 => flags.delivery_report_requested = true,
                    18 => flags.delete_report_requested = true,
                    b => log::info!(
                        "Parsing bundle primary block with reserved flag bit {} set",
                        b
                    ),
                }
            }
        }
        if f & !((2 ^ 20) - 1) != 0 {
            log::info!(
                "Parsing bundle primary block with unassigned flag bit {} set",
                f
            );
        }
        flags
    }

    pub fn as_u64(&self) -> u64 {
        let mut flags: u64 = 0;
        if self.is_fragment {
            flags |= 1 << 0;
        }
        if self.is_admin_record {
            flags |= 1 << 1;
        }
        if self.do_not_fragment {
            flags |= 1 << 2;
        }
        if self.app_ack_requested {
            flags |= 1 << 5;
        }
        if self.status_time_requested {
            flags |= 1 << 6;
        }
        if self.receipt_report_requested {
            flags |= 1 << 14;
        }
        if self.forward_report_requested {
            flags |= 1 << 16;
        }
        if self.delivery_report_requested {
            flags |= 1 << 17;
        }
        if self.delete_report_requested {
            flags |= 1 << 18;
        }
        flags
    }
}

impl cbor::decode::FromCbor for BundleFlags {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (flags, o, tags) = cbor::decode::parse_detail(data)?;
        Ok((BundleFlags::new(flags), o, tags))
    }
}

pub enum Eid {
    Null,
    LocalNode {
        service_number: u32,
    },
    Ipn {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Dtn {
        node_name: String,
        demux: String,
    },
}

impl Eid {
    fn parse_dtn_eid(value: cbor::decode::Value) -> Result<Eid, anyhow::Error> {
        match value {
            cbor::decode::Value::Uint(0) => Ok(Self::Null),
            cbor::decode::Value::Text("none", _) => {
                log::info!("Parsing dtn EID 'none'");
                Ok(Self::Null)
            }
            cbor::decode::Value::Text(s, _) => {
                if !s.is_ascii() {
                    Err(anyhow!("dtn URI be ASCII"))
                } else if !s.starts_with("//") {
                    Err(anyhow!("dtn URI must start with '//'"))
                } else if let Some((s1, s2)) = &s[2..].split_once('/') {
                    Ok(Self::Dtn {
                        node_name: s1.to_string(),
                        demux: s2.to_string(),
                    })
                } else {
                    Err(anyhow!("dtn URI missing name-delim '/'"))
                }
            }
            _ => Err(anyhow!("dtn URI is not a CBOR text string or 0")),
        }
    }

    fn parse_ipn_eid(mut value: cbor::decode::Array) -> Result<Eid, anyhow::Error> {
        if let Some(count) = value.count() {
            if !(2..=3).contains(&count) {
                return Err(anyhow!(
                    "IPN EIDs must be encoded as 2 or 3 element CBOR arrays"
                ));
            }
        } else {
            log::info!("Parsing IPN EID as indefinite array");
        }

        let v1 = value.parse::<u64>()?;
        let v2 = value.parse::<u64>()?;

        let (allocator_id, node_number, service_number) = if let Some(v3) =
            value.try_parse::<u64>()?
        {
            if (v1 >= 2 ^ 32) || (v2 >= 2 ^ 32) || (v3 >= 2 ^ 32) {
                return Err(anyhow!(
                    "Invalid IPN EID components: {}, {}, {}",
                    v1,
                    v2,
                    v3
                ));
            }

            // Check indefinite array length
            if value.count().is_none() {
                value.parse_end_or_else(|| anyhow!("Additional items found in IPN EID array"))?;
            }

            (v1 as u32, v2 as u32, v3 as u32)
        } else {
            if v2 >= 2 ^ 32 {
                return Err(anyhow!("Invalid IPN EID service number {}", v2));
            }
            ((v1 >> 32) as u32, (v1 & ((2 ^ 32) - 1)) as u32, v2 as u32)
        };

        if allocator_id == 0 && node_number == 0 {
            if service_number != 0 {
                log::info!("Null EID with service number {}", service_number)
            }
            Ok(Self::Null)
        } else if allocator_id == 0 && node_number == (2 ^ 32) - 1 {
            Ok(Self::LocalNode { service_number })
        } else {
            Ok(Self::Ipn {
                allocator_id,
                node_number,
                service_number,
            })
        }
    }
}

impl cbor::encode::ToCbor for &Eid {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        cbor::encode::write_with_tags(
            match self {
                Eid::Null => vec![cbor::encode::write(1u8), cbor::encode::write(0u8)],
                Eid::Dtn { node_name, demux } => vec![
                    cbor::encode::write(1u8),
                    cbor::encode::write(
                        ["/", node_name.as_str(), demux.as_str()].join("/"),
                    ),
                ],
                Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number,
                } if *allocator_id == 0 => vec![
                    cbor::encode::write(2u8),
                    cbor::encode::write(vec![
                        cbor::encode::write(*node_number),
                        cbor::encode::write(*service_number),
                    ]),
                ],
                Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number,
                } => vec![
                    cbor::encode::write(2u8),
                    cbor::encode::write(vec![
                        cbor::encode::write(*allocator_id),
                        cbor::encode::write(*node_number),
                        cbor::encode::write(*service_number),
                    ]),
                ],
                Eid::LocalNode { service_number } => vec![
                    cbor::encode::write(2u8),
                    cbor::encode::write(vec![
                        cbor::encode::write((2u64 ^ 32) - 1),
                        cbor::encode::write(*service_number),
                    ]),
                ],
            },
            tags,
        )
    }
}

impl cbor::decode::FromCbor for Eid {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        cbor::decode::parse_value(data, |value, tags| {
            if let cbor::decode::Value::Array(mut a) = value {
                if !tags.is_empty() {
                    log::info!("Parsing EID with tags");
                }
                match a.count() {
                    None => log::info!("Parsing EID array of indefinite length"),
                    Some(count) if count != 2 => {
                        return Err(anyhow!("EID is not encoded as a 2 element CBOR array"))
                    }
                    _ => {}
                }
                let schema = a.parse::<u64>()?;
                let eid = a.try_parse_item(|value: cbor::decode::Value<'_>, _, tags2| {
                    if !tags2.is_empty() {
                        log::info!("Parsing EID value with tags");
                    }
                    match (schema, value) {
                        (1, value) => Self::parse_dtn_eid(value),
                        (2, cbor::decode::Value::Array(a)) => Self::parse_ipn_eid(a),
                        (2, _) => Err(anyhow!("IPN EIDs must be encoded as a CBOR array")),
                        _ => Err(anyhow!("Unsupported EID scheme {}", schema)),
                    }
                })?;

                if a.count().is_none() {
                    a.parse_end_or_else(|| anyhow!("Additional items found in EID array"))?;
                }
                Ok((eid, tags.to_vec()))
            } else {
                Err(anyhow!("EID is not encoded as a CBOR array"))
            }
        })
        .map(|((eid, tags), o)| (eid, o, tags))
    }
}

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

pub enum BlockType {
    Payload,
    PreviousNode,
    BundleAge,
    HopCount,
    Private(u64),
}

impl BlockType {
    pub fn new(code: u64) -> Result<Self, anyhow::Error> {
        match code {
            0 => Err(anyhow!("Extension block type code 0 is reserved")),
            1 => Ok(BlockType::Payload),
            6 => Ok(BlockType::PreviousNode),
            7 => Ok(BlockType::BundleAge),
            10 => Ok(BlockType::HopCount),
            _ => {
                if !(192..=255).contains(&code) {
                    log::info!("Extension block uses unassigned type code {}", code);
                }
                Ok(BlockType::Private(code))
            }
        }
    }

    pub fn as_u64(&self) -> u64 {
        match self {
            BlockType::Payload => 1,
            BlockType::PreviousNode => 6,
            BlockType::BundleAge => 7,
            BlockType::HopCount => 10,
            BlockType::Private(v) => *v,
        }
    }
}

impl cbor::decode::FromCbor for BlockType {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (code, o, tags) = cbor::decode::parse_detail(data)?;
        Ok((BlockType::new(code)?, o, tags))
    }
}

#[derive(Default)]
pub struct BlockFlags {
    pub must_replicate: bool,
    pub report_on_failure: bool,
    pub delete_bundle_on_failure: bool,
    pub delete_block_on_failure: bool,
}

impl BlockFlags {
    pub fn new(f: u64) -> Self {
        let mut flags = BlockFlags::default();
        for b in 0..=6 {
            if f & (1 << b) != 0 {
                match b {
                    0 => flags.must_replicate = true,
                    1 => flags.report_on_failure = true,
                    2 => flags.delete_bundle_on_failure = true,
                    4 => flags.delete_block_on_failure = true,
                    b => log::info!("Parsing bundle block with reserved flag bit {} set", b),
                }
            }
        }
        if f & !((2 ^ 6) - 1) != 0 {
            log::info!("Parsing bundle block with unassigned flag bit {} set", f);
        }
        flags
    }

    pub fn as_u64(&self) -> u64 {
        let mut flags: u64 = 0;
        if self.must_replicate {
            flags |= 1 << 0;
        }
        if self.report_on_failure {
            flags |= 1 << 1;
        }
        if self.delete_bundle_on_failure {
            flags |= 1 << 2;
        }
        if self.delete_block_on_failure {
            flags |= 1 << 4;
        }
        flags
    }
}

impl cbor::decode::FromCbor for BlockFlags {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (flags, o, tags) = cbor::decode::parse_detail(data)?;
        Ok((BlockFlags::new(flags), o, tags))
    }
}

pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub data_offset: Option<usize>,
}

pub struct Bundle {
    pub primary: PrimaryBlock,
    pub extensions: HashMap<u64, Block>,
}

pub fn parse(data: &[u8]) -> Result<Bundle, anyhow::Error> {
    let (b, consumed) = cbor::decode::parse_value(data, |value, tags| {
        if let cbor::decode::Value::Array(a) = value {
            if !tags.is_empty() {
                log::info!("Parsing bundle with tags");
            }
            parse_bundle_blocks(data, a)
        } else {
            Err(anyhow!("Bundle is not a CBOR array"))
        }
    })?;
    if consumed < data.len() {
        return Err(anyhow!(
            "Bundle has additional data after end of CBOR array"
        ));
    }
    Ok(b)
}

fn parse_bundle_blocks(
    data: &[u8],
    mut blocks: cbor::decode::Array,
) -> Result<Bundle, anyhow::Error> {
    // Parse Primary block
    let primary = blocks.try_parse_item(|value, block_start, tags| {
        if let cbor::decode::Value::Array(a) = value {
            if !tags.is_empty() {
                log::info!("Parsing primary block with tags");
            }
            parse_primary_block(data, a, block_start)
        } else {
            Err(anyhow!("Bundle primary block is not a CBOR array"))
        }
    })?;

    // Parse other blocks
    let extensions = {
        // Use an intermediate vector so we can check the payload was the last item
        let mut extension_blocks = Vec::new();
        loop {
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

                // Check for duplicates

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
        }
    };

    Ok(Bundle {
        primary,
        extensions,
    })
}

fn parse_primary_block(
    data: &[u8],
    mut block: cbor::decode::Array,
    block_start: usize,
) -> Result<PrimaryBlock, anyhow::Error> {
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
    let crc_type = block.parse::<u64>()?;

    // Parse EIDs
    let dest_eid = block.parse::<Eid>()?;
    if let Eid::Null = &dest_eid {
        return Err(anyhow!("Bundle has Null destination EID"));
    };
    let source_eid = block.parse::<Eid>()?;
    let report_to_eid = block.parse::<Eid>()?;

    // Parse timestamp
    let timestamp = parse_timestamp(&mut block)?;

    // Parse lifetime
    let lifetime = block.parse::<u64>()?;

    // Parse fragment parts
    let fragment_info = if !flags.is_fragment {
        None
    } else {
        let offset = block.parse::<u64>()?;
        let total_len = block.parse::<u64>()?;
        Some(FragmentInfo { offset, total_len })
    };

    // Check CRC
    parse_crc_value(data, block_start, block, crc_type)?;

    Ok(PrimaryBlock {
        flags,
        source: source_eid,
        destination: dest_eid,
        report_to: report_to_eid,
        timestamp,
        lifetime,
        fragment_info,
    })
}

fn parse_timestamp(block: &mut cbor::decode::Array) -> Result<(u64, u64), anyhow::Error> {
    block.try_parse_item(|value, _, tags| {
        if let cbor::decode::Value::Array(mut a) = value {
            if !tags.is_empty() {
                log::info!("Parsing bundle primary block timestamp with tags");
            }

            let creation_time = a.parse::<u64>()?;
            if !tags.is_empty() {
                log::info!("Parsing bundle primary block timestamp with tags");
            }

            let seq_no = a.parse::<u64>()?;
            if !tags.is_empty() {
                log::info!("Parsing bundle primary block timestamp with tags");
            }

            Ok((creation_time, seq_no))
        } else {
            Err(anyhow!(
                "Bundle primary block timestamp must be a CBOR array"
            ))
        }
    })
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
                    log::info!("Parsing bundle primary block CRC value with tags");
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
