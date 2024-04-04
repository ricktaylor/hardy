use anyhow::anyhow;
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

pub fn parse(data: &[u8]) -> Result<(Bundle, bool), anyhow::Error> {
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
    let (primary, valid) = blocks.try_parse_item(|value, block_start, tags| {
        if let cbor::decode::Value::Array(a) = value {
            if !tags.is_empty() {
                log::info!("Parsing primary block with tags");
            }
            primary_block::parse(data, a, block_start)
        } else {
            Err(anyhow!("Bundle primary block is not a CBOR array"))
        }
    })?;

    let (extensions, valid) = if valid {
        // Parse other blocks
        match block::parse_extension_blocks(data, blocks) {
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

pub fn dtn_time(instant: &time::OffsetDateTime) -> u64 {
    (*instant - time::macros::datetime!(2000-01-01 00:00:00 UTC)).whole_milliseconds() as u64
}
