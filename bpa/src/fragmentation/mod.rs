use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;
use time::OffsetDateTime;

use crate::{Arc, Bytes};
mod coverage;
mod reassembly;

pub(crate) use coverage::Coverage;
pub(crate) use reassembly::{FragmentResult, process_fragment};

/// Describes a fragment being recorded for reassembly.
pub struct FragmentDescriptor<'a> {
    pub source: &'a Eid,
    pub timestamp: &'a CreationTimestamp,
    pub total_adu_length: u64,
    pub offset: u64,
    pub length: u64,
    pub extension_blocks: Option<&'a Bytes>,
    pub expiry: OffsetDateTime,
}

/// Result returned by `Store::receive_fragment`.
pub enum ReassemblyStatus {
    /// Fragment recorded, waiting for more.
    Pending,
    /// All bytes written, ready to finalize.
    Complete {
        /// Storage name of the completed ADU.
        storage_name: Arc<str>,
        /// Fragment 0's wire data for primary block reconstruction.
        extension_blocks: Option<Bytes>,
    },
}
