use anyhow::anyhow;
use hardy_cbor as cbor;

mod block_flags;
mod block_type;
mod bundle_flags;
mod bundle_status;
mod crc;
mod creation_timestamp;
mod eid;

pub struct Metadata {
    pub status: BundleStatus,
    pub storage_name: String,
    pub hash: Vec<u8>,
    pub received_at: Option<time::OffsetDateTime>,
}

pub struct BundleId {
    pub source: Eid,
    pub timestamp: CreationTimestamp,
    pub fragment_info: Option<FragmentInfo>,
}

#[derive(Copy, Clone)]
pub struct FragmentInfo {
    pub offset: u64,
    pub total_len: u64,
}

pub struct Bundle {
    pub id: BundleId,
    pub flags: BundleFlags,
    pub crc_type: CrcType,
    pub destination: Eid,
    pub report_to: Eid,
    pub lifetime: u64,
    pub blocks: std::collections::HashMap<u64, Block>,
}

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
pub use bundle_status::BundleStatus;
pub use crc::CrcType;
pub use creation_timestamp::CreationTimestamp;
pub use eid::Eid;
