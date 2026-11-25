use hardy_bpv7::{creation_timestamp::CreationTimestamp, eid::Eid};

/// A bundle's position in the dispatch, forward and deliver lifecycle.
///
/// Metadata storage persists it, so processing resumes after a restart, but
/// never through serde: backends encode it in their own typed columns and
/// re-impose it through
/// [`StoredBundle::into_bundle`](super::StoredBundle::into_bundle).
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub enum BundleStatus {
    /// Freshly received, not yet processed.
    #[default]
    New,
    /// Queued for dispatch. The queue's consumer claims the bundle into
    /// [`Dispatching`](Self::Dispatching) on dequeue, and the storage
    /// poller recovers only bundles still in this status, so an in-flight
    /// bundle is never re-queued as a duplicate.
    DispatchPending,
    /// A routing decision is in flight. The status is transient: the
    /// dispatch consumer claims a bundle into it, and the routing outcome
    /// moves the bundle straight on, to a queue, a park, reassembly or a
    /// tombstone.
    Dispatching,
    /// Queued for forwarding to a specific CLA peer.
    ForwardPending {
        /// Identifier of the CLA peer this bundle is queued for.
        peer: u32,
        /// Index of the policy queue within the peer's egress queues,
        /// from 0 to `FlowControllerFactory::queue_count()`; queue 0
        /// always exists.
        queue: u32,
        /// The adjacency EID the routing decision resolved. It belongs to
        /// the queue-assignment record rather than to the queue's identity,
        /// see [`same_queue`](Self::same_queue), so the egress channel's
        /// recovery re-delivers the decision intact.
        next_hop: Eid,
    },
    /// Offered to a CLA that has taken ownership of the transfer, and held
    /// until the CLA reports the outcome through `Sink::transfer_outcome`
    /// or the peer is removed. The reaper defers expiry in this status,
    /// since the transfer cannot be recalled from the wire, so an expired
    /// bundle resolves when the outcome arrives: a completed transfer
    /// reports truthfully, and any other exit is dropped as
    /// `LifetimeExpired` at the dispatch expiry checkpoint.
    ForwardAckPending {
        /// Identifier of the CLA peer the transfer was accepted for.
        peer: u32,
    },
    /// Queued for delivery to a local service, the local counterpart of
    /// [`ForwardPending`](Self::ForwardPending). The service's delivery
    /// channel holds it, and it is swept to
    /// [`WaitingForService`](Self::WaitingForService) when the service
    /// unregisters or the BPA restarts.
    DeliverPending {
        /// Canonical registration EID of the service this bundle is queued for.
        service: Eid,
    },
    /// Offered to a local service through `Service::deliver`, the local
    /// counterpart of [`ForwardAckPending`](Self::ForwardAckPending). No
    /// storage poller recovers this status: every delivery exit resolves
    /// the claim, and a restart re-parks it as
    /// [`WaitingForService`](Self::WaitingForService).
    DeliveryAckPending {
        /// Canonical registration EID of the service the bundle was offered to.
        service: Eid,
    },
    /// Fragment of an Application Data Unit awaiting reassembly.
    AduFragment {
        /// Source EID of the unfragmented bundle.
        source: Eid,
        /// Creation timestamp of the original bundle, used as a reassembly key.
        timestamp: CreationTimestamp,
    },
    /// Awaiting a future forwarding opportunity, a scheduled contact for
    /// instance.
    Waiting,
    /// Parked awaiting a service registration for this endpoint, the local
    /// counterpart of [`Waiting`](Self::Waiting). Dispatch or a delivery
    /// exit sets it on finding no registered service, as do the unregister
    /// sweep and the restart re-park, and the registration-time poll
    /// recovers it.
    WaitingForService {
        /// Canonical registration EID of the service the bundle awaits.
        service: Eid,
    },
}

impl BundleStatus {
    /// Whether the expiry reaper defers a bundle in this status.
    ///
    /// An in-flight hand-off cannot be recalled from the wire or from the
    /// service, so expiring it would report a deletion that never happened.
    /// Each hand-off resolves its own outcome, and every non-terminal exit
    /// re-arms the reaper watch. The match is exhaustive, so a new status
    /// chooses a side here rather than falling through to being reaped.
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

    /// Whether two statuses name the same queue, ignoring any per-bundle
    /// routing payload the assignment record carries, such as
    /// [`ForwardPending::next_hop`](Self::ForwardPending).
    ///
    /// The storage channels recover queued bundles by this relation, a peer
    /// queue holding bundles whose resolved adjacencies differ, while the
    /// conditional status swaps keep to full equality: a swap arbitrates
    /// ownership of one bundle, whose snapshot carries its own payload.
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
