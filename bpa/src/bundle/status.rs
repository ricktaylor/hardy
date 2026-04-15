use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Processing status of a bundle within the BPA pipeline.
///
/// Tracks where a bundle is in the dispatch/forward/deliver lifecycle.
/// Persisted to metadata storage so processing can resume after restart.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum BundleStatus {
    /// Freshly received, not yet processed.
    #[default]
    New,
    /// Currently being dispatched (routing lookup and forwarding decision).
    Dispatching,
    /// Queued for forwarding to a specific CLA peer.
    ForwardPending {
        /// Identifier of the CLA peer this bundle is queued for.
        peer: u32,
        /// Optional queue index within the peer's egress queues.
        queue: Option<u32>,
    },
    /// Fragment of an Application Data Unit awaiting reassembly.
    AduFragment {
        /// Source EID of the original (unfragmented) bundle.
        source: Eid,
        /// Creation timestamp of the original bundle, used as a reassembly key.
        timestamp: CreationTimestamp,
    },
    /// Waiting for a future forwarding opportunity (e.g., scheduled contact).
    Waiting,
    /// Delivered to a local service and awaiting its response or acknowledgement.
    WaitingForService {
        /// EID of the service that is processing this bundle.
        service: Eid,
    },
}
