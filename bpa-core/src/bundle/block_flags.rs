use super::*;

#[derive(Default)]
pub struct BlockFlags {
    pub must_replicate: bool,
    pub report_on_failure: bool,
    pub delete_bundle_on_failure: bool,
    pub delete_block_on_failure: bool,
}

impl BlockFlags {
    pub fn new(f: u64) -> Self {
        let mut flags = BlockFlags::default();
        for b in 0..=6 {
            if f & (1 << b) != 0 {
                match b {
                    0 => flags.must_replicate = true,
                    1 => flags.report_on_failure = true,
                    2 => flags.delete_bundle_on_failure = true,
                    4 => flags.delete_block_on_failure = true,
                    b => log::info!("Parsing bundle block with reserved flag bit {} set", b),
                }
            }
        }
        if f & !((2 ^ 6) - 1) != 0 {
            log::info!(
                "Parsing bundle block with unassigned flag bits set: {:#x}",
                f
            );
        }
        flags
    }

    pub fn as_u64(&self) -> u64 {
        let mut flags: u64 = 0;
        if self.must_replicate {
            flags |= 1 << 0;
        }
        if self.report_on_failure {
            flags |= 1 << 1;
        }
        if self.delete_bundle_on_failure {
            flags |= 1 << 2;
        }
        if self.delete_block_on_failure {
            flags |= 1 << 4;
        }
        flags
    }
}

impl cbor::decode::FromCbor for BlockFlags {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (flags, o, tags) = cbor::decode::parse_detail(data)?;
        Ok((BlockFlags::new(flags), o, tags))
    }
}
