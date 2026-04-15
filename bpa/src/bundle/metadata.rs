use hardy_bpv7::eid::Eid;
use hardy_bpv7::eid::NodeId;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::status::BundleStatus;
use crate::Arc;
use crate::cla::ClaAddress;

/// Immutable ingress context captured when a bundle is first received.
///
/// These fields are set once at ingress and never modified during
/// processing. Transient fields (`ingress_cla`, `next_hop`) are not
/// persisted to storage.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ReadOnlyMetadata {
    /// Wall-clock time when the bundle was received by this BPA.
    pub received_at: OffsetDateTime,
    /// Node ID of the peer that forwarded this bundle (from CLA handshake).
    pub ingress_peer_node: Option<NodeId>,
    /// Convergence-layer address of the ingress peer.
    pub ingress_peer_addr: Option<ClaAddress>,
    /// Name of the CLA instance that received this bundle (transient, not persisted).
    #[cfg_attr(feature = "serde", serde(skip))]
    pub ingress_cla: Option<Arc<str>>,
    /// Next-hop EID resolved by the RIB during forwarding (transient, not persisted).
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

/// Mutable annotations that filters may modify during bundle processing.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct WritableMetadata {
    /// Optional flow label for QoS differentiation.
    pub flow_label: Option<u32>,
    // TODO: Add a 'trace' mark that will trigger local feedback
}

/// Combined metadata for a bundle held in the BPA.
///
/// Groups the storage key, processing status, immutable ingress context,
/// and filter-writable annotations into a single structure.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct BundleMetadata {
    /// Opaque key used by the storage backend to locate the serialised bundle data.
    pub(crate) storage_name: Option<Arc<str>>,
    /// Current processing status of this bundle within the BPA pipeline.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub status: BundleStatus,
    /// Immutable ingress context set at reception time.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub read_only: ReadOnlyMetadata,
    /// Mutable annotations that filters may update during processing.
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
