#[derive(Default, Debug, Copy, Clone)]
pub struct BlockFlags {
    pub must_replicate: bool,
    pub report_on_failure: bool,
    pub delete_bundle_on_failure: bool,
    pub delete_block_on_failure: bool,
    pub unrecognised: u64,
}

impl From<BlockFlags> for u64 {
    fn from(value: BlockFlags) -> Self {
        let mut flags = value.unrecognised;
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
        let mut flags = Self {
            unrecognised: value & !((2 ^ 6) - 1),
            ..Default::default()
        };

        for b in 0..=6 {
            if value & (1 << b) != 0 {
                match b {
                    0 => flags.must_replicate = true,
                    1 => flags.report_on_failure = true,
                    2 => flags.delete_bundle_on_failure = true,
                    4 => flags.delete_block_on_failure = true,
                    b => {
                        flags.unrecognised |= 1 << b;
                    }
                }
            }
        }
        flags
    }
}
