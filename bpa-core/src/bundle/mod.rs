use anyhow::anyhow;
use hardy_cbor as cbor;

mod block;
mod block_flags;
mod block_type;
mod bundle_core;
mod bundle_flags;
mod bundle_status;
mod crc;
mod eid;
mod metadata;

pub use block::Block;
pub use block_flags::BlockFlags;
pub use block_type::BlockType;
pub use bundle_core::{Bundle, BundleId, FragmentInfo};
pub use bundle_flags::BundleFlags;
pub use bundle_status::BundleStatus;
pub use crc::{emit_crc_value, CrcType};
pub use eid::Eid;
pub use metadata::Metadata;

pub fn dtn_time(instant: &time::OffsetDateTime) -> u64 {
    (*instant - time::macros::datetime!(2000-01-01 00:00:00 UTC)).whole_milliseconds() as u64
}
