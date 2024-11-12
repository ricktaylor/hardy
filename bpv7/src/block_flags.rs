use super::*;

#[derive(Default, Debug, Clone)]
pub struct BlockFlags {
    pub must_replicate: bool,
    pub report_on_failure: bool,
    pub delete_bundle_on_failure: bool,
    pub delete_block_on_failure: bool,
    pub unrecognised: u64,
}

impl From<&BlockFlags> for u64 {
    fn from(value: &BlockFlags) -> Self {
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

impl cbor::encode::ToCbor for &BlockFlags {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(u64::from(self))
    }
}

impl cbor::decode::FromCbor for BlockFlags {
    type Error = cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse::<(u64, bool, usize)>(data)
            .map(|o| o.map(|(value, shortest, len)| (value.into(), shortest, len)))
    }
}
