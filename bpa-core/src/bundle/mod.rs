use hardy_cbor as cbor;
use tracing::*;

mod block;
mod block_flags;
mod block_type;
mod bundle_flags;
mod bundle_id;
mod bundle_status;
mod bundle_type;
mod crc;
mod creation_timestamp;
mod eid;
mod eid_pattern;
mod eid_pattern_map;
mod metadata;

pub use block::Block;
pub use block_flags::BlockFlags;
pub use block_type::BlockType;
pub use bundle_flags::BundleFlags;
pub use bundle_id::{BundleId, FragmentInfo};
pub use bundle_status::BundleStatus;
pub use crc::CrcType;
pub use creation_timestamp::CreationTimestamp;
pub use eid::{Eid, EidError};
pub use eid_pattern::EidPattern;
pub use eid_pattern_map::EidPatternMap;
pub use metadata::Metadata;

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
