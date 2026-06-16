use super::status::BundleStatus;
use crate::{Arc, cla::ClaAddress};
use hardy_bpv7::{
    eid::{Eid, NodeId},
    hop_info::HopInfo,
};
use time::OffsetDateTime;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Ingress context and decoded extension-block fields captured when a
/// bundle's content is parsed.
///
/// The ingress fields are set once at reception. The extension-block fields
/// (`previous_node` / `age` / `hop_count`) are decoded from the bundle once
/// when its content is parsed — at ingress, on local build, or when a write
/// filter re-parses a rewritten bundle (see [`crate::bundle::parse`]). They
/// are read-only to the rest of the pipeline. Transient fields (`ingress_cla`,
/// `next_hop`) are not persisted to storage.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ReadOnlyMetadata {
    /// Wall-clock time when the bundle was received by this BPA.
    pub received_at: OffsetDateTime,
    /// Node ID of the peer that forwarded this bundle (from CLA handshake).
    pub ingress_peer_node: Option<NodeId>,
    /// Convergence-layer address of the ingress peer.
    pub ingress_peer_addr: Option<ClaAddress>,
    /// EID of the node that last forwarded the bundle (Previous Node block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub previous_node: Option<Eid>,
    /// Age of the bundle, used when the source node has no clock (Bundle Age block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub age: Option<core::time::Duration>,
    /// Hop limit and current hop count for the bundle (Hop Count block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub hop_count: Option<HopInfo>,
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
            previous_node: None,
            age: None,
            hop_count: None,
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
