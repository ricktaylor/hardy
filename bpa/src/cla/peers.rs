use core::num::NonZeroU32;

use hardy_async::Semaphore;
use hardy_bpv7::eid::Eid;
use tracing::warn;

use super::*;
use crate::lanes::seat;

// PeerTable uses hardy_async::sync::spin::RwLock because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. Read-heavy pattern (forward is called frequently)
// 3. No blocking/iteration while holding lock
// 4. Avoids OS rwlock overhead on hot forwarding path

// The egress seat's settings, the same for every peer: the depth of each
// queue's in-memory channel, and the cap on any one peer's concurrent
// forwards, whatever its CLA declares.
#[derive(Clone, Copy)]
pub struct EgressConfig {
    pub poll_channel_depth: usize,
    pub lane_cap: NonZeroU32,
}

pub struct Peer {
    // The id every queue below is keyed under.
    id: u32,
    // One poller per policy queue, indexed by the queue index — queue 0
    // always exists (`FlowControllerFactory::queue_count` is non-zero). The
    // pollers share the peer's lanes.
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
    ///
    /// The peer keeps up to its effective lanes of forwards in flight: one
    /// for a serial CLA, a limited CLA's count bounded by the egress lane
    /// cap, or the cap for an unlimited one, shared by all of the peer's
    /// queue pollers, so the declaration holds whatever the policy's queue
    /// count.
    pub async fn start(
        config: EgressConfig,
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
                cla.lanes,
            ))
            .await;

        let lanes = seat::budget(seat::effective(cla.lanes, config.lane_cap));
        let queue_count = cla.policy.queue_count().get();
        let mut queues = Vec::with_capacity(queue_count as usize);
        for q in 0..queue_count {
            queues.push(Self::start_queue_poller(
                config.poll_channel_depth,
                lanes.clone(),
                controller.clone(),
                store.clone(),
                tasks,
                peer,
                q,
            ));
        }

        Arc::new(Self {
            id: peer,
            queues,
            controller,
        })
    }

    // Spawn a queue's poller. Each received bundle takes a lane of the
    // peer's budget, and its `controller.forward` runs as a task of its own
    // holding that lane, so the peer's queues together keep at most the
    // budget's offers in flight; while every lane is busy the poller stops
    // pulling, and the queue's channel fills and spills to storage as
    // behind a serial poller. Offers start in queue order and may complete
    // out of order. Once the queue closes and drains, the poller exits only
    // after its in-flight forwards have finished.
    fn start_queue_poller(
        poll_channel_depth: usize,
        lanes: Semaphore,
        controller: Arc<dyn policy::FlowController>,
        store: Arc<storage::store::Store>,
        tasks: &hardy_async::TaskPool,
        peer: u32,
        queue: u32,
    ) -> storage::channel::Sender {
        // The channel key is the queue's identity; the adjacency is
        // per-bundle payload, so the key carries a placeholder never
        // matched (see BundleStatus::same_queue) and never stored.
        let (tx, rx) = store.channel(
            bundle::BundleStatus::ForwardPending {
                peer,
                queue,
                next_hop: Eid::Null,
            },
            poll_channel_depth,
        );

        let forwards = tasks.clone();
        hardy_async::spawn!(
            tasks,
            "egress_queue_poller",
            (peer = peer, queue = queue),
            async move { Self::drain_queue(rx, lanes, controller, forwards, peer, queue).await }
        );

        tx
    }

    // The queue poller's body: each bundle received takes a lane, and its
    // `controller.forward` runs on `forwards` holding the lane. Returns once
    // the queue has closed and drained and every forward has finished.
    // `peer` only labels the forwards' spans, which exist with `instrument`.
    #[cfg_attr(not(feature = "instrument"), allow(unused_variables))]
    async fn drain_queue(
        rx: hardy_async::closeable::Receiver<bundle::Bundle>,
        lanes: Semaphore,
        controller: Arc<dyn policy::FlowController>,
        forwards: hardy_async::TaskPool,
        peer: u32,
        queue: u32,
    ) {
        seat::drain(&rx, &lanes, |bundle, lane| {
            let controller = controller.clone();
            hardy_async::spawn!(
                forwards,
                "egress_forward",
                (peer = peer, queue = queue),
                async move {
                    controller.forward(queue, bundle).await;
                    drop(lane);
                }
            )
        })
        .await
    }

    // Err(bundle) deliberately hands ownership back to the caller; boxing
    // the bundle to shrink the Err variant would tax every call site.
    #[allow(clippy::result_large_err)]
    pub async fn forward(
        &self,
        next_hop: Eid,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        // The per-peer controller owns the queue assignment; nothing on
        // this path touches shared policy state.
        let queue = self.controller.queue_for();
        // An out-of-range index is a policy bug: clamp to queue 0, which
        // always exists.
        let queue = if (queue as usize) < self.queues.len() {
            queue
        } else {
            warn!("Egress policy classified a bundle into out-of-range queue {queue}");
            0
        };

        // The full assignment record: this queue's identity plus the
        // resolved adjacency, so the decision survives the channel's
        // storage spill.
        let status = bundle::BundleStatus::ForwardPending {
            peer: self.id,
            queue,
            next_hop,
        };

        match self.queues[queue as usize].send_to(bundle, status).await {
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
        next_hop: Eid,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        // sync::spin::RwLock::read() returns guard directly (no Result)
        let Some(peer) = self.inner.read().peers.get(&peer_id).cloned() else {
            return Err(bundle);
        };

        peer.forward(next_hop, bundle).await
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

    // // TODO: Implement test for 'Queue Selection' (Verify the controller's queue_for() selects the peer queue)
    // #[test]
    // fn test_queue_selection() {
    //     todo!("Verify the controller's queue_for() selects the peer queue");
    // }

    use core::{future::Future, pin::pin};

    use futures::poll;
    use hardy_async::closeable;

    use super::*;
    use crate::bundle::tests::test_bundle;

    // A controller whose forwards announce their bundle, hold until the
    // test releases one, then announce they finished. flume stays
    // qualified: its channel types share names with the `closeable` ones.
    struct HoldController {
        entered_tx: flume::Sender<hardy_bpv7::bundle::Id>,
        release_rx: flume::Receiver<()>,
        finished_tx: flume::Sender<hardy_bpv7::bundle::Id>,
    }

    #[async_trait]
    impl policy::FlowController for HoldController {
        fn queue_for(&self) -> u32 {
            0
        }

        async fn forward(&self, _queue: u32, bundle: bundle::Bundle) {
            let _ = self.entered_tx.send(bundle.id().clone());
            let _ = self.release_rx.recv_async().await;
            let _ = self.finished_tx.send(bundle.id().clone());
        }
    }

    struct Rig {
        tx: closeable::Sender<bundle::Bundle>,
        rx: closeable::Receiver<bundle::Bundle>,
        controller: Arc<HoldController>,
        entered_rx: flume::Receiver<hardy_bpv7::bundle::Id>,
        release_tx: flume::Sender<()>,
        finished_rx: flume::Receiver<hardy_bpv7::bundle::Id>,
    }

    // A queue's in-memory channel — what the poller drains — with `count`
    // bundles already queued in it.
    async fn rig(count: u32) -> Rig {
        let (tx, rx) = closeable::bounded(16);
        for n in 0..count {
            tx.send(test_bundle(&format!("ipn:0.{}.1", n + 2), "ipn:0.99.1"))
                .await
                .unwrap();
        }
        let (entered_tx, entered_rx) = flume::unbounded();
        let (release_tx, release_rx) = flume::unbounded();
        let (finished_tx, finished_rx) = flume::unbounded();
        Rig {
            tx,
            rx,
            controller: Arc::new(HoldController {
                entered_tx,
                release_rx,
                finished_tx,
            }),
            entered_rx,
            release_tx,
            finished_rx,
        }
    }

    // Poll the poller until `n` forwards have entered the controller,
    // yielding so the spawned forwards run; the timeout only bounds a
    // regression.
    async fn drive_until_entered<P: Future<Output = ()> + Unpin>(
        poller: &mut P,
        entered: &flume::Receiver<hardy_bpv7::bundle::Id>,
        n: usize,
    ) {
        tokio::time::timeout(tokio::time::Duration::from_secs(10), async {
            while entered.len() < n {
                assert!(poll!(&mut *poller).is_pending());
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the timeout only bounds a regression");
        assert!(poll!(&mut *poller).is_pending());
    }

    // Event-driven wait; the timeout only bounds a regression.
    async fn recv<T>(rx: &flume::Receiver<T>) -> T {
        tokio::time::timeout(tokio::time::Duration::from_secs(10), rx.recv_async())
            .await
            .expect("the timeout only bounds a regression")
            .unwrap()
    }

    // With every lane held, the poller leaves the rest of the queue in its
    // channel: exactly as many bundles left the channel as there are
    // lanes, and a freed lane pulls exactly one more.
    #[tokio::test]
    async fn the_poller_pulls_no_bundle_without_a_free_lane() {
        let rig = rig(5).await;
        let forwards = hardy_async::TaskPool::new();
        let poller = Peer::drain_queue(
            rig.rx,
            seat::budget(NonZeroU32::new(2).unwrap()),
            rig.controller.clone(),
            forwards.clone(),
            1,
            0,
        );
        let mut poller = pin!(poller);

        drive_until_entered(&mut poller, &rig.entered_rx, 2).await;
        assert_eq!(rig.tx.len(), 3, "only the two held bundles left");

        rig.release_tx.send(()).unwrap();
        recv(&rig.finished_rx).await;
        drive_until_entered(&mut poller, &rig.entered_rx, 3).await;
        assert_eq!(rig.tx.len(), 2, "the freed lane pulled one bundle");

        drop(rig.release_tx);
        rig.tx.close();
        poller.await;
        forwards.shutdown().await;
    }

    // A closed queue's poller returns only once its in-flight forwards have
    // finished: closing never abandons a forward mid-way.
    #[tokio::test]
    async fn a_closed_queue_poller_waits_for_its_forwards() {
        let rig = rig(2).await;
        let forwards = hardy_async::TaskPool::new();
        let poller = Peer::drain_queue(
            rig.rx,
            seat::budget(NonZeroU32::new(2).unwrap()),
            rig.controller.clone(),
            forwards.clone(),
            1,
            0,
        );
        let mut poller = pin!(poller);

        drive_until_entered(&mut poller, &rig.entered_rx, 2).await;
        rig.tx.close();
        assert!(
            poll!(poller.as_mut()).is_pending(),
            "both forwards are still in flight"
        );

        rig.release_tx.send(()).unwrap();
        recv(&rig.finished_rx).await;
        assert!(
            poll!(poller.as_mut()).is_pending(),
            "one forward is still in flight"
        );

        rig.release_tx.send(()).unwrap();
        // The timeout only bounds a regression: the poller returns once the
        // last forward has finished.
        tokio::time::timeout(tokio::time::Duration::from_secs(10), poller)
            .await
            .expect("the poller returns once its forwards finish");
        assert_eq!(rig.finished_rx.len(), 1, "the last forward finished");

        forwards.shutdown().await;
    }

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
