use super::*;
use crc::Crc;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    BadCBOR(#[from] cbor::decode::Error),

    #[error("Extra bytes after CBOR")]
    ExtraBytes,

    #[error("Invalid bundle format")]
    InvalidFormat,

    #[error("Invalid primary block format")]
    InvalidPrimaryBlock,

    #[error("Invalid version {0}")]
    InvalidVersion(u64),

    #[error("Invalid CRC type {0}")]
    InvalidCRCType(u64),

    #[error("Invalid EID")]
    InvalidEID,

    #[error("Bad CRC")]
    BadCRC,
}

#[derive(Default)]
pub struct Flags {
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

impl Flags {
    fn new(f: u64) -> Self {
        let mut flags = Flags::default();
        for b in 0..20 {
            if f & (1 << b) != 0 {
                match b {
                    1 => flags.is_fragment = true,
                    2 => flags.is_admin_record = true,
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
}

pub enum CRCType {
    None,
    CRC16,
    CRC32,
}

pub enum Eid {
    None,
    LocalNode(u32),
    Ipn(u32, u32, u32),
    Dtn(String, String),
}

pub struct FragmentInfo {
    pub offset: u64,
    pub total_len: u64,
}

pub struct PrimaryBlock {
    pub flags: Flags,
    pub crc_type: CRCType,
    pub source: Eid,
    pub destination: Eid,
    pub report_to: Eid,
    pub timestamp: (u64, u64),
    pub lifetime: u64,
    pub fragment_info: Option<FragmentInfo>,
}

pub struct Bundle {}

impl Bundle {
    pub fn new(data: &[u8]) -> Result<Self, anyhow::Error> {
        let (_, consumed) = cbor::decode::parse(data, |value, tags| {
            if let cbor::decode::Value::Array(a) = value {
                if tags.is_some() {
                    log::info!("Parsing bundle with tags");
                }
                process_bundle_blocks(data, a)
            } else {
                Err(Error::InvalidFormat.into())
            }
        })?;
        if consumed < data.len() {
            return Err(Error::ExtraBytes.into());
        }

        todo!()
    }
}

fn process_bundle_blocks(
    data: &[u8],
    mut blocks: cbor::decode::Array,
) -> Result<(), anyhow::Error> {
    if blocks.count().is_some() {
        log::info!("Parsing bundle as fixed length array");
    }

    // Process Primary block
    let primary = blocks.try_parse_item(|value, _, block_start, tags| {
        if let cbor::decode::Value::Array(a) = value {
            if tags.is_some() {
                log::info!("Parsing primary block with tags");
            }
            process_primary_block(data, a, block_start)
        } else {
            Err(Error::InvalidFormat.into())
        }
    })?;

    todo!()
}

fn process_primary_block(
    data: &[u8],
    mut block: cbor::decode::Array,
    block_start: usize,
) -> Result<PrimaryBlock, anyhow::Error> {
    // Check number of items in the array
    match block.count() {
        None => log::info!("Parsing primary block of indefinite length"),
        Some(count) if !(8..=11).contains(&count) => return Err(Error::InvalidPrimaryBlock.into()),
        _ => {}
    }

    // Check version
    let (version, tags) = block.parse_uint()?;
    if version != 7 {
        return Err(Error::InvalidVersion(version).into());
    } else if tags.is_some() {
        log::info!("Parsing bundle primary block version with tags");
    }

    // Parse flags
    let (flags, tags) = block.parse_uint()?;
    if tags.is_some() {
        log::info!("Parsing bundle primary block flags with tags");
    }
    let flags = Flags::new(flags);

    // Parse CRC Type
    let (crc_type, tags) = block.parse_uint()?;
    if tags.is_some() {
        log::info!("Parsing bundle primary block crc type with tags");
    }
    let crc_type = match crc_type {
        0 => CRCType::None,
        1 => CRCType::CRC16,
        2 => CRCType::CRC32,
        _ => return Err(Error::InvalidCRCType(crc_type).into()),
    };

    // Parse EIDs
    let dest_eid = parse_eid(&mut block)?;
    let source_eid = parse_eid(&mut block)?;
    let report_to_eid = parse_eid(&mut block)?;

    // Parse timestamp
    let timestamp = parse_timestamp(&mut block)?;

    // Parse lifetime
    let (lifetime, tags) = block.parse_uint()?;
    if tags.is_some() {
        log::info!("Parsing bundle primary block lifetime with tags");
    }

    // Parse fragment parts
    let fragment_info = if !flags.is_fragment {
        None
    } else {
        let (offset, tags) = block.parse_uint()?;
        if tags.is_some() {
            log::info!("Parsing bundle primary block fragment offset with tags");
        }
        let (total_len, tags) = block.parse_uint()?;
        if tags.is_some() {
            log::info!("Parsing bundle primary block total application data unit length with tags");
        }
        Some(FragmentInfo { offset, total_len })
    };

    // Parse CRC
    let crc_info = block.try_parse_item(|value, _, crc_start, tags| match value {
        cbor::decode::Value::End(_) => {
            if let CRCType::None = crc_type {
                Ok(None)
            } else {
                Err(Error::InvalidFormat.into())
            }
        }
        cbor::decode::Value::Uint(crc) => {
            if let CRCType::None = crc_type {
                Err(Error::InvalidFormat.into())
            } else {
                if tags.is_some() {
                    log::info!("Parsing bundle primary block CRC value with tags");
                }
                Ok(Some((crc, crc_start)))
            }
        }
        _ => Err(Error::InvalidFormat.into()),
    })?;

    // Confirm we are at the end of the block
    let (crc_end, block_end) = block.try_parse_item(|value, _, start, _| match value {
        cbor::decode::Value::End(end) => Ok((start, end)),
        _ => Err(Error::InvalidFormat.into()),
    })?;

    // Now check CRC
    if let Some((crc_value, crc_start)) = crc_info {
        if let CRCType::CRC16 = crc_type {
            const X25: Crc<u16> = Crc::<u16>::new(&crc::CRC_16_IBM_SDLC);
            let mut digest = X25.digest();
            digest.update(&data[block_start..crc_start]);
            digest.update(&vec![0; crc_end - crc_start]);
            if block_end > crc_end {
                digest.update(&data[crc_end..block_end]);
            }
            if crc_value != digest.finalize() as u64 {
                return Err(Error::BadCRC.into());
            }
        } else if let CRCType::CRC32 = crc_type {
            pub const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&crc::CRC_32_ISCSI);
            let mut digest = CASTAGNOLI.digest();
            digest.update(&data[block_start..crc_start]);
            digest.update(&vec![0; crc_end - crc_start]);
            if block_end > crc_end {
                digest.update(&data[crc_end..block_end]);
            }
            if crc_value != digest.finalize() as u64 {
                return Err(Error::BadCRC.into());
            }
        }
    }

    Ok(PrimaryBlock {
        flags,
        crc_type,
        source: source_eid,
        destination: dest_eid,
        report_to: report_to_eid,
        timestamp,
        lifetime,
        fragment_info,
    })
}

fn parse_eid(block: &mut cbor::decode::Array) -> Result<Eid, anyhow::Error> {
    block.try_parse_item(|value, _, _, tags| {
        if let cbor::decode::Value::Array(mut a) = value {
            if tags.is_some() {
                log::info!("Parsing EID with tags");
            }
            match a.count() {
                None => log::info!("Parsing EID of indefinite length"),
                Some(count) if count != 2 => return Err(Error::InvalidEID.into()),
                _ => {}
            }
            let (schema, tags) = a.parse_uint()?;
            if tags.is_some() {
                log::info!("Parsing EID schema with tags");
            }
            match schema {
                1 | 2 => a.try_parse_item(|value: cbor::decode::Value<'_>, _, _, tags| {
                    if tags.is_some() {
                        log::info!("Parsing EID value with tags");
                    }
                    match (schema, value) {
                        (1, value) => parse_dtn_eid(value),
                        (2, cbor::decode::Value::Array(a)) => parse_ipn_eid(a),
                        _ => unreachable!(),
                    }
                }),
                _ => Err(Error::InvalidEID.into()),
            }
        } else {
            Err(Error::InvalidEID.into())
        }
    })
}

fn parse_dtn_eid(value: cbor::decode::Value) -> Result<Eid, anyhow::Error> {
    match value {
        cbor::decode::Value::Uint(0) => Ok(Eid::None),
        cbor::decode::Value::String("none") => {
            log::info!("Parsing dtn EID 'none'");
            Ok(Eid::None)
        }
        cbor::decode::Value::String(s) if s.starts_with("//") => {
            if let Some((s1, s2)) = &s[2..].split_once('/') {
                Ok(Eid::Dtn(s1.to_string(), s2.to_string()))
            } else {
                Err(Error::InvalidEID.into())
            }
        }
        _ => Err(Error::InvalidEID.into()),
    }
}

fn parse_ipn_eid(mut value: cbor::decode::Array) -> Result<Eid, anyhow::Error> {
    let (v1, tags) = value.parse_uint()?;
    if tags.is_some() {
        log::info!("Parsing IPN EID with tags");
    }

    let (v2, tags) = value.parse_uint()?;
    if tags.is_some() {
        log::info!("Parsing IPN EID with tags");
    }

    let v3 = value.try_parse_item(|value, _, _, tags| {
        if tags.is_some() {
            log::info!("Parsing IPN EID with tags");
        }
        match value {
            cbor::decode::Value::Uint(value) => Ok(Some(value)),
            cbor::decode::Value::End(_) => Ok(None),
            _ => Err(cbor::decode::Error::IncorrectType.into()),
        }
    })?;

    let (allocator_id, node_num, service_num) = if let Some(v3) = v3 {
        if (v1 >= 2 ^ 32) || (v2 >= 2 ^ 32) || (v3 >= 2 ^ 32) {
            return Err(Error::InvalidEID.into());
        }
        (v1 as u32, v2 as u32, v3 as u32)
    } else {
        if v2 >= 2 ^ 32 {
            return Err(Error::InvalidEID.into());
        }
        ((v1 >> 32) as u32, (v1 & ((2 ^ 32) - 1)) as u32, v2 as u32)
    };

    if allocator_id == 0 && node_num == (2 ^ 32) - 1 {
        Ok(Eid::LocalNode(service_num))
    } else {
        Ok(Eid::Ipn(allocator_id, node_num, service_num))
    }
}

fn parse_timestamp(block: &mut cbor::decode::Array) -> Result<(u64, u64), anyhow::Error> {
    block.try_parse_item(|value, _, _, tags| {
        if let cbor::decode::Value::Array(mut a) = value {
            if tags.is_some() {
                log::info!("Parsing bundle primary block timestamp with tags");
            }

            let (creation_time, tags) = a.parse_uint()?;
            if tags.is_some() {
                log::info!("Parsing bundle primary block timestamp with tags");
            }

            let (seq_no, tags) = a.parse_uint()?;
            if tags.is_some() {
                log::info!("Parsing bundle primary block timestamp with tags");
            }

            Ok((creation_time, seq_no))
        } else {
            Err(Error::InvalidFormat.into())
        }
    })
}
