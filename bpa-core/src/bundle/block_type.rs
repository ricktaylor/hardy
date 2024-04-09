use super::*;

#[derive(Copy, Clone)]
pub enum BlockType {
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

impl TryFrom<u64> for BlockType {
    type Error = anyhow::Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Err(anyhow!("Extension block type code 0 is reserved")),
            1 => Ok(BlockType::Payload),
            6 => Ok(BlockType::PreviousNode),
            7 => Ok(BlockType::BundleAge),
            10 => Ok(BlockType::HopCount),
            11 => Ok(BlockType::BlockIntegrity),
            12 => Ok(BlockType::BlockSecurity),
            _ => {
                if value <= 191 {
                    log::info!("Extension block uses unassigned type code {}", value);
                }
                Ok(BlockType::Private(value))
            }
        }
    }
}

impl cbor::decode::FromCbor for BlockType {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (code, o, tags) = cbor::decode::parse_detail::<u64>(data)?;
        Ok((code.try_into()?, o, tags))
    }
}

impl cbor::encode::ToCbor for BlockType {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        cbor::encode::emit_with_tags::<u64>(self.into(), tags)
    }
}
