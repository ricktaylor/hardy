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

/// The read-only metadata
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ReadOnlyMetadata {
    /// When the bundle was received
    pub received_at: time::OffsetDateTime,
    /// The node that sent this bundle
    pub ingress_peer_node: Option<NodeId>,
    /// The CLA address of the peer
    pub ingress_peer_addr: Option<cla::ClaAddress>,
    /// The CLA that received this bundle (transient)
    #[cfg_attr(feature = "serde", serde(skip))]
    pub ingress_cla: Option<Arc<str>>,

    // Transient routing context (not persisted, set during RIB lookup)
    #[cfg_attr(feature = "serde", serde(skip))]
    pub next_hop: Option<Eid>,
}

impl Default for ReadOnlyMetadata {
    fn default() -> Self {
        Self {
            received_at: time::OffsetDateTime::now_utc(),
            ingress_peer_node: None,
            ingress_peer_addr: None,
            ingress_cla: None,
            next_hop: None,
        }
    }
}

/// The metadata that may be editted by WriteFilters
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WritableMetadata {
    /// Flow label for QoS
    pub flow_label: Option<u32>,
    // TODO: Add a 'trace' mark that will trigger local feedback
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BundleMetadata {
    /// Storage identifier for bundle data
    pub(crate) storage_name: Option<Arc<str>>,
    /// Current processing status
    pub status: BundleStatus,
    /// Immutable ingress context
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub read_only: ReadOnlyMetadata,
    /// Filter-modifiable annotations
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub writable: WritableMetadata,
}

impl Default for BundleMetadata {
    fn default() -> Self {
        Self {
            storage_name: None,
            status: BundleStatus::New,
            read_only: ReadOnlyMetadata::default(),
            writable: WritableMetadata::default(),
        }
    }
}
