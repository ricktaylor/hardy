use hardy_cbor as cbor;
use tracing::*;

mod block_flags;
mod block_type;
mod bundle_flags;
mod bundle_id;
mod bundle_status;
mod crc;
mod creation_timestamp;
mod eid;

#[derive(Debug)]
pub struct Metadata {
    pub status: BundleStatus,
    pub storage_name: String,
    pub hash: Vec<u8>,
    pub received_at: Option<time::OffsetDateTime>,
}

#[derive(Debug, Copy, Clone)]
pub struct HopInfo {
    pub count: u64,
    pub limit: u64,
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

#[derive(Debug)]
pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub crc_type: CrcType,
    pub data_offset: usize,
    pub data_len: usize,
}

pub use block_flags::BlockFlags;
pub use block_type::BlockType;
pub use bundle_flags::BundleFlags;
pub use bundle_id::{BundleId, FragmentInfo};
pub use bundle_status::BundleStatus;
pub use crc::CrcType;
pub use creation_timestamp::CreationTimestamp;
pub use eid::Eid;
