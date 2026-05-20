//! The rich BPv7 bundle representation the BPA stores: the structural
//! bundle with the well-known extension blocks interpreted into typed
//! fields. Interpreting those blocks is a BPA concern, so this type and
//! its companion extractor ([`crate::bp7_parse::extract_extension_block_fields`])
//! live here rather than in `hardy-bpv7`. This is a pure data struct —
//! payload access / BPSec decryption is done by the free helpers in
//! [`crate::bp7_parse`].

use crate::HashMap;
use hardy_bpv7::{block, bundle, crc, eid, hop_info};

/// Represents a complete BPv7 bundle.
///
/// Holds the primary-block fields, the values unpacked from known
/// extension blocks (`previous_node` / `age` / `hop_count`), and a map
/// of all blocks present in the bundle. The raw wire bytes are stored
/// separately by the BPA; payload access goes through
/// [`crate::bp7_parse::block_data`].
#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Bpv7Bundle {
    // From Primary Block
    /// The unique identifier for the bundle.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub id: bundle::Id,

    /// The bundle-specific processing control flags.
    pub flags: bundle::Flags,
    /// The type of CRC used for the primary block's integrity check.
    pub crc_type: crc::CrcType,
    /// The EID of the bundle's final destination.
    pub destination: eid::Eid,
    /// The EID to which status reports should be sent.
    pub report_to: eid::Eid,
    /// The time duration after which the bundle should be considered expired.
    pub lifetime: core::time::Duration,

    // Unpacked from extension blocks
    /// The EID of the node that last forwarded the bundle.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub previous_node: Option<eid::Eid>,

    /// The age of the bundle, used if the source node has no clock.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub age: Option<core::time::Duration>,

    /// The hop limit and current hop count for the bundle.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub hop_count: Option<hop_info::HopInfo>,

    /// A map of all blocks in the bundle, keyed by their block number.
    pub blocks: HashMap<u64, block::Block>,
}
