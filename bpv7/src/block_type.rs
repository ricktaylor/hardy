use super::*;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockType {
    Primary,
    Payload,
    PreviousNode,
    BundleAge,
    HopCount,
    BlockIntegrity,
    BlockSecurity,
    Private(u64),
}

impl From<BlockType> for u64 {
    fn from(value: BlockType) -> Self {
        match value {
            BlockType::Primary => 0,
            BlockType::Payload => 1,
            BlockType::PreviousNode => 6,
            BlockType::BundleAge => 7,
            BlockType::HopCount => 10,
            BlockType::BlockIntegrity => 11,
            BlockType::BlockSecurity => 12,
            BlockType::Private(v) => v,
        }
    }
}

impl From<u64> for BlockType {
    fn from(value: u64) -> Self {
        match value {
            0 => BlockType::Primary,
            1 => BlockType::Payload,
            6 => BlockType::PreviousNode,
            7 => BlockType::BundleAge,
            10 => BlockType::HopCount,
            11 => BlockType::BlockIntegrity,
            12 => BlockType::BlockSecurity,
            _ => {
                if value <= 191 {
                    trace!("Extension block uses unassigned type code {value}");
                }
                BlockType::Private(value)
            }
        }
    }
}
