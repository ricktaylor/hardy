use super::*;

#[derive(Default, Debug, Copy, Clone)]
pub struct BlockFlags {
    pub must_replicate: bool,
    pub report_on_failure: bool,
    pub delete_bundle_on_failure: bool,
    pub delete_block_on_failure: bool,
}

impl From<BlockFlags> for u64 {
    fn from(value: BlockFlags) -> Self {
        let mut flags: u64 = 0;
        if value.must_replicate {
            flags |= 1 << 0;
        }
        if value.report_on_failure {
            flags |= 1 << 1;
        }
        if value.delete_bundle_on_failure {
            flags |= 1 << 2;
        }
        if value.delete_block_on_failure {
            flags |= 1 << 4;
        }
        flags
    }
}

impl From<u64> for BlockFlags {
    fn from(value: u64) -> Self {
        let mut flags = BlockFlags::default();
        for b in 0..=6 {
            if value & (1 << b) != 0 {
                match b {
                    0 => flags.must_replicate = true,
                    1 => flags.report_on_failure = true,
                    2 => flags.delete_bundle_on_failure = true,
                    4 => flags.delete_block_on_failure = true,
                    b => trace!("Parsing bundle block with reserved flag bit {b} set"),
                }
            }
        }
        if value & !((2 ^ 6) - 1) != 0 {
            trace!("Parsing bundle block with unassigned flag bits set: {value:#x}");
        }
        flags
    }
}

impl cbor::encode::ToCbor for BlockFlags {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit::<u64>(self.into())
    }
}

impl cbor::decode::FromCbor for BlockFlags {
    type Error = cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), Self::Error> {
        cbor::decode::parse_detail::<u64>(data).map(|(v, len, tags)| (v.into(), len, tags))
    }
}
