use super::*;
use hardy_bpv7::{creation_timestamp::CreationTimestamp, eid::Eid};

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BundleStatus {
    Dispatching,
    ForwardPending {
        peer: u32,
        queue: Option<u32>,
    },
    LocalPending {
        service: u32,
    },
    AduFragment {
        source: Eid,
        timestamp: CreationTimestamp,
    },
    Waiting,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BundleMetadata {
    pub(crate) storage_name: Option<Arc<str>>,

    pub status: BundleStatus,
    pub received_at: time::OffsetDateTime,
    pub non_canonical: bool,
    pub flow_label: Option<u32>,
    // TODO: Add a 'trace' mark that will trigger local feedback
}

impl Default for BundleMetadata {
    fn default() -> Self {
        Self {
            storage_name: None,
            status: BundleStatus::Dispatching,
            received_at: time::OffsetDateTime::now_utc(),
            non_canonical: false,
            flow_label: None,
        }
    }
}
