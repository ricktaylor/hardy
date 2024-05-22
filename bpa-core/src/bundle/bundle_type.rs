use super::*;

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
