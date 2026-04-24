use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Processing status of a bundle within the BPA pipeline.
///
/// Only statuses that require persistence are represented here.
/// In-flight forwarding is handled in memory — no intermediate
/// statuses are written to storage on the hot path.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum BundleStatus {
    /// Freshly received or persisted, awaiting dispatch.
    #[default]
    New,
    /// Forwarding failed or route not yet available, retry later.
    Waiting,
    /// Fragment of an Application Data Unit awaiting reassembly.
    AduFragment {
        /// Source EID of the original (unfragmented) bundle.
        source: Eid,
        /// Creation timestamp of the original bundle, used as a reassembly key.
        timestamp: CreationTimestamp,
    },
    /// Delivered to a local service and awaiting its response or acknowledgement.
    WaitingForService {
        /// EID of the service that is processing this bundle.
        service: Eid,
    },
}
