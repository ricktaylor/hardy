/*!
The BPv7 block processing control flags (RFC 9171 Section 4.2.2) and their
wire bit mapping.
*/

use hardy_cbor::{
    decode::FromCbor,
    encode::{Encoder, ToCbor},
};

use crate::{Error, canonical::parse_canonical};

/// Represents the processing control flags for a BPv7 block.
///
/// These flags, defined in RFC 9171 Section 4.2.2, control how a node should
/// process the block, especially in cases of failure or fragmentation.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockFlags {
    /// If set, the block must be replicated in every fragment of the bundle.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub must_replicate: bool,
    /// If set, a status report should be generated if block processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub report_on_failure: bool,
    /// If set, the entire bundle should be deleted if block processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delete_bundle_on_failure: bool,
    /// If set, this block should be deleted if its processing fails.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delete_block_on_failure: bool,

    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    /// A bitmask of any unrecognized flags encountered during parsing.
    pub unrecognised: Option<u64>,
}

impl BlockFlags {
    /// The processing-control flags for a primary block (RFC 9171 §4.2.3):
    /// must-replicate, report-on-failure, and delete-bundle-on-failure set.
    pub fn primary() -> Self {
        Self {
            must_replicate: true,
            report_on_failure: true,
            delete_bundle_on_failure: true,
            delete_block_on_failure: false,
            unrecognised: None,
        }
    }
}

impl From<&BlockFlags> for u64 {
    fn from(value: &BlockFlags) -> Self {
        let mut flags = value.unrecognised.unwrap_or(0);
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
        let mut flags = Self::default();
        let mut unrecognised = value;

        if (value & 1) != 0 {
            flags.must_replicate = true;
            unrecognised &= !1;
        }
        if (value & 2) != 0 {
            flags.report_on_failure = true;
            unrecognised &= !2;
        }
        if (value & 4) != 0 {
            flags.delete_bundle_on_failure = true;
            unrecognised &= !4;
        }
        if (value & 16) != 0 {
            flags.delete_block_on_failure = true;
            unrecognised &= !16;
        }

        if unrecognised != 0 {
            flags.unrecognised = Some(unrecognised);
        }
        flags
    }
}

impl ToCbor for BlockFlags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(&u64::from(self))
    }
}

impl FromCbor for BlockFlags {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, len) = parse_canonical::<u64, _>(data, Error::NotCanonical)?;
        Ok((value.into(), true, len))
    }
}
