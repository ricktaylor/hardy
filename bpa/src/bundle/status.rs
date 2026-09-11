use hardy_bpv7::{CreationTimestamp, eid::Eid};

/// Processing status of a bundle within the BPA pipeline.
///
/// Tracks where a bundle is in the dispatch/forward/deliver lifecycle.
/// Persisted to metadata storage so processing can resume after restart —
/// but never through serde: backends encode it in their own typed columns
/// and re-impose it via
/// [`StoredBundle::into_bundle`](super::StoredBundle::into_bundle).
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub enum BundleStatus {
    /// Freshly received, not yet processed.
    #[default]
    New,
    /// Queued for dispatch processing. The dispatch queue consumer claims the
    /// bundle to [`Dispatching`](Self::Dispatching) on dequeue; the storage
    /// poller only recovers bundles still in this status, so an in-flight
    /// bundle cannot be re-queued as a duplicate.
    DispatchPending,
    /// Routing decision in flight. Transient: the dispatch consumer claims a
    /// bundle into this status, and the routing outcome immediately moves it
    /// on (a queue, a park, reassembly, or a tombstone).
    Dispatching,
    /// Queued for forwarding to a specific CLA peer.
    ForwardPending {
        /// Identifier of the CLA peer this bundle is queued for.
        peer: u32,
        /// The policy queue index within the peer's egress queues
        /// (`0..FlowControllerFactory::queue_count()`; queue 0 always exists).
        queue: u32,
    },
    /// Offered to a CLA that has taken ownership of the transfer; retained
    /// until the CLA reports the outcome via `Sink::transfer_outcome` or the
    /// peer is removed. The reaper defers expiry of this status — the
    /// transfer cannot be recalled from the wire — so an expired bundle
    /// resolves when the outcome arrives: a completed transfer reports
    /// truthfully, and any other exit is dropped as `LifetimeExpired` at the
    /// dispatch expiry checkpoint.
    ForwardAckPending {
        /// Identifier of the CLA peer the transfer was accepted for.
        peer: u32,
    },
    /// Queued for delivery to a specific local service (the local analogue
    /// of [`ForwardPending`](Self::ForwardPending)). Held in the service's
    /// delivery channel; swept to
    /// [`WaitingForService`](Self::WaitingForService) when the service
    /// unregisters or the BPA restarts.
    DeliverPending {
        /// Canonical registration EID of the service this bundle is queued for.
        service: Eid,
    },
    /// Offered to a local service via `on_deliver` (the local analogue of
    /// [`ForwardAckPending`](Self::ForwardAckPending)). No storage poller
    /// recovers this status: every delivery exit resolves the claim, and a
    /// restart re-parks it as
    /// [`WaitingForService`](Self::WaitingForService).
    DeliveryAckPending {
        /// Canonical registration EID of the service the bundle was offered to.
        service: Eid,
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
    /// Parked awaiting a service registration for this endpoint (the local
    /// analogue of [`Waiting`](Self::Waiting)): set when dispatch or a
    /// delivery exit finds no registered service, and by the unregister
    /// sweep and restart re-park. Recovered by the registration-time poll.
    WaitingForService {
        /// Canonical registration EID of the service the bundle awaits.
        service: Eid,
    },
}

impl BundleStatus {
    /// Whether the expiry reaper defers a bundle in this status: an
    /// in-flight hand-off cannot be recalled from the wire or the service,
    /// so expiring it would report a deletion that did not happen. Each
    /// hand-off resolves its own outcome, and every non-terminal exit
    /// re-arms the reaper watch. The match is exhaustive so a new status
    /// must choose a side here, not fall into "reap it" by omission.
    pub(crate) fn defers_expiry(&self) -> bool {
        match self {
            Self::ForwardAckPending { .. } | Self::DeliveryAckPending { .. } => true,
            Self::New
            | Self::DispatchPending
            | Self::Dispatching
            | Self::ForwardPending { .. }
            | Self::DeliverPending { .. }
            | Self::AduFragment { .. }
            | Self::Waiting
            | Self::WaitingForService { .. } => false,
        }
    }
}
