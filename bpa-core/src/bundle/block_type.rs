use super::*;

pub enum BlockType {
    Payload,
    PreviousNode,
    BundleAge,
    HopCount,
    Private(u64),
}

impl BlockType {
    pub fn new(code: u64) -> Result<Self, anyhow::Error> {
        match code {
            0 => Err(anyhow!("Extension block type code 0 is reserved")),
            1 => Ok(BlockType::Payload),
            6 => Ok(BlockType::PreviousNode),
            7 => Ok(BlockType::BundleAge),
            10 => Ok(BlockType::HopCount),
            _ => {
                if !(192..=255).contains(&code) {
                    log::info!("Extension block uses unassigned type code {}", code);
                }
                Ok(BlockType::Private(code))
            }
        }
    }

    pub fn as_u64(&self) -> u64 {
        match self {
            BlockType::Payload => 1,
            BlockType::PreviousNode => 6,
            BlockType::BundleAge => 7,
            BlockType::HopCount => 10,
            BlockType::Private(v) => *v,
        }
    }
}

impl cbor::decode::FromCbor for BlockType {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (code, o, tags) = cbor::decode::parse_detail(data)?;
        Ok((BlockType::new(code)?, o, tags))
    }
}
