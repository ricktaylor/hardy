use super::*;

#[derive(Default, Debug)]
pub struct Bundle {
    // From Primary Block
    pub id: prelude::BundleId,
    pub flags: prelude::BundleFlags,
    pub crc_type: prelude::CrcType,
    pub destination: prelude::Eid,
    pub report_to: prelude::Eid,
    pub lifetime: u64,

    // Unpacked from extension blocks
    pub previous_node: Option<prelude::Eid>,
    pub age: Option<u64>,
    pub hop_count: Option<HopInfo>,

    // The extension blocks
    pub blocks: std::collections::HashMap<u64, prelude::Block>,
}

#[derive(Debug, Copy, Clone)]
pub struct HopInfo {
    pub count: u64,
    pub limit: u64,
}
