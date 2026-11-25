use hardy_async::{TaskPool, sync::spin::RwLock};
use hardy_bpv7::eid::Eid;
use tracing::warn;

use super::{Address, queue, table};
use crate::{
    Arc, HashMap, HashSet,
    bundle::{Bundle, BundleStatus},
    dispatcher::Dispatcher,
    policy::FlowController,
    storage::{
        channel::{SendError, Sender},
        store::Store,
    },
};

// The table is behind a spin lock. Every critical section is an O(1) lookup
// or insert, awaiting, blocking and iterating nothing, so the lock is never
// held long enough to repay an OS rwlock's park, and the read side is the
// hot forwarding path.

pub struct Peer {
    // One poller per policy queue, indexed by the queue index. Queue 0
    // always exists, `FlowControllerFactory::queue_count` being non-zero.
    queues: Vec<Sender>,
    // This peer's controller owns the queue assignment, through
    // `queue_for`, so the hot forwarding path touches no shared policy
    // state.
    controller: Arc<dyn FlowController>,
}

impl Peer {
    /// Builds the peer, controller and per-queue pollers together, ready
    /// to forward.
    ///
    /// [`PeerTable::publish`] publishes it only after construction
    /// returns, so a reachable `Peer` is a working one and there is no
    /// half-built state to guard against.
    pub async fn start(
        poll_channel_depth: usize,
        cla: Arc<table::Record>,
        peer: u32,
        cla_addr: Address,
        store: Arc<Store>,
        dispatcher: Arc<Dispatcher>,
        tasks: &TaskPool,
    ) -> Arc<Self> {
        let controller = cla
            .policy
            .new_controller(queue::new_set(
                cla.cla.clone(),
                dispatcher,
                peer,
                cla_addr,
                cla.terms.lane_count,
            ))
            .await;

        let queue_count = cla.policy.queue_count().get();
        let mut queues = Vec::with_capacity(queue_count as usize);
        for q in 0..queue_count {
            queues.push(Self::start_queue_poller(
                poll_channel_depth,
                controller.clone(),
                store.clone(),
                tasks,
                peer,
                q,
            ));
        }

        Arc::new(Self { queues, controller })
    }

    fn start_queue_poller(
        poll_channel_depth: usize,
        controller: Arc<dyn FlowController>,
        store: Arc<Store>,
        tasks: &TaskPool,
        peer: u32,
        queue: u32,
    ) -> Sender {
        // The channel key is the queue's identity, and the adjacency is
        // per-bundle payload, so the key carries a placeholder that is
        // never matched, see `BundleStatus::same_queue`, and never stored.
        let (tx, rx) = store.channel(
            BundleStatus::ForwardPending {
                peer,
                queue,
                next_hop: Eid::Null,
            },
            poll_channel_depth,
        );

        hardy_async::spawn!(
            tasks,
            "egress_queue_poller",
            (peer = peer, queue = queue),
            async move {
                while let Ok(bundle) = rx.recv().await {
                    controller.forward(queue, bundle).await;
                }
            }
        );

        tx
    }

    #[expect(
        clippy::result_large_err,
        reason = "Err(bundle) hands ownership back to the caller; boxing it to shrink the Err variant would allocate on every call"
    )]
    pub async fn forward(&self, next_hop: Eid, bundle: Bundle) -> core::result::Result<(), Bundle> {
        // The per-peer controller owns the queue assignment, so nothing on
        // this path touches shared policy state.
        let queue = self.controller.queue_for();
        // An out-of-range index is a policy bug, clamped to queue 0, which
        // always exists.
        let queue = self.queues.get(queue as usize).unwrap_or_else(|| {
            warn!("Egress policy classified a bundle into out-of-range queue {queue}");
            &self.queues[0]
        });

        // The full assignment record, this queue's identity, which the
        // selection above may have fallen back on, plus the resolved
        // adjacency, so the decision survives the channel's storage spill.
        let BundleStatus::ForwardPending { peer, queue: q, .. } = queue.queue_status() else {
            unreachable!("Egress queue with a non-ForwardPending target status")
        };
        let status = BundleStatus::ForwardPending {
            peer: *peer,
            queue: *q,
            next_hop,
        };

        match queue.send_to(bundle, status).await {
            Ok(_) => Ok(()),
            Err(SendError(b)) => Err(b),
        }
    }

    fn close(&self) {
        for tx in &self.queues {
            tx.close();
        }
    }
}

#[derive(Default)]
struct PeerTableInner {
    peers: HashMap<u32, Arc<Peer>>,
    // Ids minted by `reserve` but not yet published. The `Reservation`
    // clears them, through `publish` or, for an abandoned claim, its
    // `Drop`. `remove` never touches this set, so a concurrent removal
    // cannot let `reserve` re-mint an id whose peer is still under
    // construction.
    reserved: HashSet<u32>,
    next: u32,
}

pub struct PeerTable {
    inner: RwLock<PeerTableInner>,
}

impl PeerTable {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(PeerTableInner::default()),
        }
    }

    /// Mints a fresh peer id without publishing anything.
    ///
    /// The returned [`Reservation`] holds the id against reuse until
    /// [`publish`](Reservation::publish) consumes it, and releases it on
    /// drop if the claim is abandoned before a peer is built.
    pub fn reserve(&self) -> Reservation<'_> {
        let mut inner = self.inner.write();
        let peer_id = loop {
            inner.next = inner.next.wrapping_add(1);
            if !inner.peers.contains_key(&inner.next) && !inner.reserved.contains(&inner.next) {
                break inner.next;
            }
        };
        inner.reserved.insert(peer_id);
        Reservation {
            table: self,
            id: peer_id,
            published: false,
        }
    }

    pub async fn remove(&self, peer_id: u32) {
        let peer = self.inner.write().peers.remove(&peer_id);

        if let Some(peer) = peer {
            peer.close();
        }
    }

    #[expect(
        clippy::result_large_err,
        reason = "Err(bundle) hands ownership back to the caller; boxing it to shrink the Err variant would allocate on every call"
    )]
    pub async fn forward(
        &self,
        peer_id: u32,
        next_hop: Eid,
        bundle: Bundle,
    ) -> core::result::Result<(), Bundle> {
        let Some(peer) = self.inner.read().peers.get(&peer_id).cloned() else {
            return Err(bundle);
        };

        peer.forward(next_hop, bundle).await
    }
}

/// A reserved peer id, released exactly once.
///
/// [`publish`](Self::publish) releases it once the peer is built, and a
/// drop, an unwind included, releases an abandoned claim. Publishing
/// consumes the reservation, so publishing twice, or using it afterwards,
/// does not compile.
#[must_use = "an unused reservation releases its id immediately"]
pub struct Reservation<'a> {
    table: &'a PeerTable,
    id: u32,
    published: bool,
}

impl Reservation<'_> {
    /// The reserved peer id.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Publishes a constructed peer under the reserved id, the only way a
    /// peer becomes reachable.
    pub fn publish(mut self, peer: Arc<Peer>) {
        let mut inner = self.table.inner.write();
        inner.reserved.remove(&self.id);
        inner.peers.insert(self.id, peer);
        self.published = true;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.published {
            self.table.inner.write().reserved.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: cover queue selection (the controller's `queue_for` picks the
    // peer queue) and the fallback to queue 0 for an out-of-range index.

    #[test]
    fn reservation_holds_the_id_until_dropped() {
        let table = PeerTable::new();

        let reservation = table.reserve();
        let id = reservation.id();
        assert!(
            table.inner.read().reserved.contains(&id),
            "the id is withheld from reuse while the reservation lives"
        );

        drop(reservation);
        assert!(
            !table.inner.read().reserved.contains(&id),
            "an abandoned claim releases its id"
        );
    }
}
