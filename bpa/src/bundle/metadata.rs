use hardy_bpv7::eid::Eid;
use hardy_bpv7::eid::NodeId;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::status::BundleStatus;
use crate::Arc;
use crate::cla::ClaAddress;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ReadOnlyMetadata {
    /// When the bundle was received
    pub received_at: OffsetDateTime,
    /// The node that sent this bundle
    pub ingress_peer_node: Option<NodeId>,
    /// The CLA address of the peer
    pub ingress_peer_addr: Option<ClaAddress>,
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
            received_at: OffsetDateTime::now_utc(),
            ingress_peer_node: None,
            ingress_peer_addr: None,
            ingress_cla: None,
            next_hop: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct WritableMetadata {
    /// Flow label for QoS
    pub flow_label: Option<u32>,
    // TODO: Add a 'trace' mark that will trigger local feedback
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct BundleMetadata {
    /// Storage identifier for bundle data
    pub(crate) storage_name: Option<Arc<str>>,
    /// Current processing status
    #[cfg_attr(feature = "serde", serde(skip))]
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
