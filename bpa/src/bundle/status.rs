use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;

/// Processing status of a bundle within the BPA pipeline.
///
/// Tracks where a bundle is in the dispatch/forward/deliver lifecycle.
/// Persisted to metadata storage so processing can resume after restart —
/// but never through serde: backends encode it in their own typed columns
/// (it is `serde(skip)`ed on [`Bundle`](super::Bundle)).
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
        /// The adjacency EID the routing decision resolved. Part of the
        /// queue-assignment record — not its identity (see
        /// [`same_queue`](Self::same_queue)) — so the egress channel's
        /// at-least-once recovery re-delivers the decision intact.
        next_hop: Eid,
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
    /// Delivered to a local service and awaiting its response or acknowledgement.
    WaitingForService {
        /// EID of the service that is processing this bundle.
        service: Eid,
    },
}

impl BundleStatus {
    /// Queue-identity equality: whether two statuses name the same queue,
    /// ignoring any per-bundle routing payload the assignment record
    /// carries ([`ForwardPending::next_hop`](Self::ForwardPending)).
    ///
    /// The storage channels recover queued bundles by this relation — a
    /// peer queue holds bundles whose resolved adjacencies differ — while
    /// the conditional status swaps keep using full equality: a swap
    /// arbitrates ownership of one bundle, whose snapshot carries its own
    /// payload.
    pub fn same_queue(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ForwardPending {
                    peer: a, queue: b, ..
                },
                Self::ForwardPending {
                    peer: x, queue: y, ..
                },
            ) => a == x && b == y,
            _ => self == other,
        }
    }
}
