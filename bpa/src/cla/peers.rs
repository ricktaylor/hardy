use tracing::warn;

use super::*;

// PeerTable uses hardy_async::sync::spin::RwLock because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. Read-heavy pattern (forward is called frequently)
// 3. No blocking/iteration while holding lock
// 4. Avoids OS rwlock overhead on hot forwarding path

pub struct Peer {
    // One poller per policy queue, indexed by the queue index — queue 0
    // always exists (`FlowControllerFactory::queue_count` is non-zero).
    queues: Vec<storage::channel::Sender>,
    // This peer's controller: owns the queue assignment (`queue_for`), so
    // the hot forwarding path touches no shared policy state.
    controller: Arc<dyn policy::FlowController>,
}

impl Peer {
    /// Builds the peer complete — controller and per-queue pollers — and
    /// returns it ready to forward. Publication into the [`PeerTable`]
    /// happens strictly after construction ([`PeerTable::publish`]), so a
    /// `Peer` that is reachable is a `Peer` that works: there is no
    /// half-built state to guard against.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        poll_channel_depth: usize,
        cla: Arc<registry::Cla>,
        peer: u32,
        cla_addr: ClaAddress,
        store: Arc<storage::store::Store>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        tasks: &hardy_async::TaskPool,
    ) -> Arc<Self> {
        let controller = cla
            .policy
            .new_controller(egress_queue::new_queue_set(
                cla.cla.clone(),
                dispatcher,
                peer,
                cla_addr,
                cla.cla.lane_count(),
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
        controller: Arc<dyn policy::FlowController>,
        store: Arc<storage::store::Store>,
        tasks: &hardy_async::TaskPool,
        peer: u32,
        queue: u32,
    ) -> storage::channel::Sender {
        let (tx, rx) = store.channel(
            bundle::BundleStatus::ForwardPending { peer, queue },
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

    // Err(bundle) deliberately hands ownership back to the caller; boxing
    // the bundle to shrink the Err variant would tax every call site.
    #[allow(clippy::result_large_err)]
    pub async fn forward(
        &self,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        // The per-peer controller owns the queue assignment; nothing on
        // this path touches shared policy state.
        let queue = self.controller.queue_for();
        // An out-of-range index is a policy bug: clamp to queue 0, which
        // always exists.
        let queue = self.queues.get(queue as usize).unwrap_or_else(|| {
            warn!("Egress policy classified a bundle into out-of-range queue {queue}");
            &self.queues[0]
        });

        match queue.send(bundle).await {
            Ok(_) => Ok(()),
            Err(storage::channel::SendError(b)) => Err(b),
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
    // Ids minted by `reserve` but not yet published. Cleared by the
    // `Reservation` — `publish` (the normal path) or its `Drop` (an
    // abandoned claim); `remove` never touches it, so a concurrent removal
    // cannot let `reserve` re-mint an id whose peer is still
    // mid-construction.
    reserved: HashSet<u32>,
    next: u32,
}

pub struct PeerTable {
    inner: hardy_async::sync::spin::RwLock<PeerTableInner>,
}

impl PeerTable {
    pub fn new() -> Self {
        Self {
            inner: hardy_async::sync::spin::RwLock::new(PeerTableInner::default()),
        }
    }

    /// Mint a fresh peer id without publishing anything: the returned
    /// [`Reservation`] holds the id against reuse until
    /// [`publish`](Reservation::publish) consumes it, or releases it on
    /// drop if the claim is abandoned before a peer is built.
    pub fn reserve(&self) -> Reservation<'_> {
        // sync::spin::RwLock::write() returns guard directly (no Result)
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

    // Err(bundle) deliberately hands ownership back to the caller; boxing
    // the bundle to shrink the Err variant would tax every call site.
    #[allow(clippy::result_large_err)]
    pub async fn forward(
        &self,
        peer_id: u32,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        // sync::spin::RwLock::read() returns guard directly (no Result)
        let Some(peer) = self.inner.read().peers.get(&peer_id).cloned() else {
            return Err(bundle);
        };

        peer.forward(bundle).await
    }
}

/// A reserved peer id: released exactly once — by
/// [`publish`](Self::publish) when the peer is built, or automatically on
/// drop (including unwind) when the claim is abandoned. Publishing consumes
/// the reservation, so a double publish or use-after-publish does not
/// compile.
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

    /// Publish a fully-constructed peer under the reserved id — the only
    /// way a peer becomes reachable, and it is complete by construction.
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
    // use super::*;

    // // TODO: Implement test for 'Queue Selection' (Verify Policy maps to correct CLA queue)
    // #[test]
    // fn test_queue_selection() {
    //     todo!("Verify Policy maps to correct CLA queue");
    // }

    // // TODO: Implement test for 'Queue Fallback' (Verify fallback to default queue on invalid index)
    // #[test]
    // fn test_queue_fallback() {
    //     todo!("Verify fallback to default queue on invalid index");
    // }

    use super::*;

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
