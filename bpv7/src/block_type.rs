use super::*;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum BlockType {
    Primary,
    Payload,
    PreviousNode,
    BundleAge,
    HopCount,
    BlockIntegrity,
    BlockSecurity,
    Unrecognised(u64),
}

impl std::fmt::Display for BlockType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockType::Primary => write!(f, "Primary"),
            BlockType::Payload => write!(f, "Payload"),
            BlockType::PreviousNode => write!(f, "Previous Node"),
            BlockType::BundleAge => write!(f, "Bundle Age"),
            BlockType::HopCount => write!(f, "Hop Count"),
            BlockType::BlockIntegrity => write!(f, "Bundle Integrity"),
            BlockType::BlockSecurity => write!(f, "Bundle Security"),
            BlockType::Unrecognised(v) if *v >= 192 && *v <= 255 => {
                write!(f, "Private/Experimental type {v}")
            }
            BlockType::Unrecognised(v) => write!(f, "Unassigned type {v}"),
        }
    }
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
            BlockType::Unrecognised(v) => v,
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
            value => BlockType::Unrecognised(value),
        }
    }
}

impl cbor::encode::ToCbor for BlockType {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(u64::from(self))
    }
}

impl cbor::decode::FromCbor for BlockType {
    type Error = cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse::<(u64, bool, usize)>(data)
            .map(|o| o.map(|(value, shortest, len)| (value.into(), shortest, len)))
    }
}
