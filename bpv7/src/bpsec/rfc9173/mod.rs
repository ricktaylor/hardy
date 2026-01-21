use super::*;
use core::ops::Range;

pub(crate) mod bcb_aes_gcm;
pub(crate) mod bib_hmac_sha2;

#[cfg(test)]
mod test;

#[derive(Debug, Hash, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ScopeFlags {
    pub include_primary_block: bool,
    pub include_target_header: bool,
    pub include_security_header: bool,
    pub unrecognised: Option<u64>,
}

impl ScopeFlags {
    pub const NONE: Self = Self {
        include_primary_block: false,
        include_target_header: false,
        include_security_header: false,
        unrecognised: None,
    };
}

impl Default for ScopeFlags {
    fn default() -> Self {
        Self {
            include_primary_block: true,
            include_target_header: true,
            include_security_header: true,
            unrecognised: None,
        }
    }
}

impl hardy_cbor::decode::FromCbor for ScopeFlags {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data).map(|(value, shortest, len)| {
            let mut flags = Self {
                include_primary_block: false,
                include_target_header: false,
                include_security_header: false,
                unrecognised: None,
            };
            let mut unrecognised = value;

            if (value & (1 << 0)) != 0 {
                flags.include_primary_block = true;
                unrecognised &= !(1 << 0);
            }
            if (value & (1 << 1)) != 0 {
                flags.include_target_header = true;
                unrecognised &= !(1 << 1);
            }
            if (value & (1 << 2)) != 0 {
                flags.include_security_header = true;
                unrecognised &= !(1 << 2);
            }

            if unrecognised != 0 {
                flags.unrecognised = Some(unrecognised);
            }
            (flags, shortest, len)
        })
    }
}

impl hardy_cbor::encode::ToCbor for ScopeFlags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        let mut flags = self.unrecognised.unwrap_or(0);
        if self.include_primary_block {
            flags |= 1 << 0;
        }
        if self.include_target_header {
            flags |= 1 << 1;
        }
        if self.include_security_header {
            flags |= 1 << 2;
        }
        encoder.emit(&flags)
    }
}
