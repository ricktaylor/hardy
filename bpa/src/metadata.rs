use super::*;
use hardy_bpv7::{creation_timestamp::CreationTimestamp, eid::Eid, eid::NodeId};

#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BundleStatus {
    #[default]
    New,
    Dispatching,
    ForwardPending {
        peer: u32,
        queue: Option<u32>,
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
    // The following are immutable after bundle creation:
    pub storage_name: Option<Arc<str>>,
    pub status: BundleStatus,
    pub received_at: time::OffsetDateTime,

    // Ingress context (set at receive time, immutable thereafter)
    pub ingress_peer_node: Option<NodeId>,
    pub ingress_peer_addr: Option<cla::ClaAddress>,
    #[cfg_attr(feature = "serde", serde(skip))]
    pub ingress_cla: Option<Arc<str>>,

    // Transient routing context (not persisted, immutable, set during RIB lookup)
    #[cfg_attr(feature = "serde", serde(skip))]
    pub next_hop: Option<Eid>,

    // The following can be updated by filters:
    pub non_canonical: bool,
    pub flow_label: Option<u32>,
    // TODO: Add a 'trace' mark that will trigger local feedback
}

impl Default for BundleMetadata {
    fn default() -> Self {
        Self {
            storage_name: None,
            status: BundleStatus::New,
            received_at: time::OffsetDateTime::now_utc(),
            ingress_peer_node: None,
            ingress_peer_addr: None,
            ingress_cla: None,
            next_hop: None,
            non_canonical: false,
            flow_label: None,
        }
    }
}
