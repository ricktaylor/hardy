use core::num::NonZeroU32;

use hardy_async::{CancellationToken, TaskPool, sync::spin::Mutex};
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, NodeId},
};
use tracing::{debug, error, info};

// `cla::Cla` and `cla::Sink` are the traits this module's `Record` and
// `Sink` are built around, so both names are taken here; the two traits
// stay module-qualified at their few use sites.
use super::{
    Acceptance, Address, ClaInit, Error, Result, Segment, TransferOutcome,
    peers::{Peer, PeerTable},
};
use crate::{
    Arc, HashMap, Weak, async_trait,
    bundle::Bundle,
    cla,
    dispatcher::Dispatcher,
    hash_map::Entry,
    node_ids::NodeIds,
    policy::{FlowControllerFactory, null_policy},
    routing::Rib,
    storage::store::Store,
};

// The maps here are behind spin locks. Every critical section is an O(1)
// lookup or insert with nothing awaited, blocked or iterated inside it, so
// the lock is never held long enough to repay an OS mutex's park.

pub struct Record {
    pub cla: Arc<dyn cla::Cla>,
    pub policy: Arc<dyn FlowControllerFactory>,
    // Snapshotted from the `ClaInit` declarations at registration.
    pub lane_count: Option<NonZeroU32>,

    name: Arc<str>,
    // Cancelled at unregistration; every in-flight stream of this
    // registration races it and fails immediately.
    cancel: CancellationToken,
    // Keyed by the link-layer adjacency, which is a peer's primary key.
    // The value pairs the node ids known for that peer with its id in the
    // `PeerTable`; an empty list is a neighbour whose EID has not been
    // learned yet, and so has no RIB entry.
    peers: Mutex<HashMap<Address, (Vec<NodeId>, u32)>>,
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Record {}

impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Record {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl core::hash::Hash for Record {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl core::fmt::Debug for Record {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Record")
            .field("name", &self.name)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}

impl Record {
    // Whether `peer_id` is one of this CLA's peers — the ownership check for
    // deferred transfer outcomes.
    pub fn owns_peer(&self, peer_id: u32) -> bool {
        self.peers.lock().values().any(|(_, id)| *id == peer_id)
    }
}

type RecordMap = HashMap<String, Arc<Record>>;

// The back-edges are weak. `Bpa` owns the registry and the dispatcher for
// the life of the BPA, so a strong edge would add no reachability — only a
// cycle through the CLA that holds this sink, and a cycle of `Arc`s never
// drops. A failed upgrade is the condition the trait already names,
// `Error::Disconnected`.
struct Sink {
    cla: Weak<Record>,
    registry: Weak<ClaRegistry>,
    dispatcher: Weak<Dispatcher>,
}

#[async_trait]
impl cla::Sink for Sink {
    async fn unregister(&self) {
        if let (Some(cla), Some(registry)) = (self.cla.upgrade(), self.registry.upgrade()) {
            registry.unregister(cla).await;
        }
    }

    async fn dispatch(
        &self,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&Address>,
        stream: &mut dyn crate::stream::Receiver<Segment>,
    ) -> Result<Acceptance> {
        let cla = self.cla.upgrade().ok_or(Error::Disconnected)?;
        let dispatcher = self.dispatcher.upgrade().ok_or(Error::Disconnected)?;

        // A CLA that unregisters mid-stream must not land its bundle: the
        // registration's token races every pull, so teardown wakes this
        // stream immediately — even parked behind a stalled producer — and
        // the transfer surfaces as cancelled. Sink-side, so the dispatcher's
        // stream consumers stay registration-agnostic.
        let mut stream = crate::stream::CancellableReceiver {
            inner: stream,
            token: cla.cancel.clone(),
        };
        let verdict = dispatcher
            .receive_bundle(cla.name.clone(), peer_node, peer_addr, &mut stream)
            .await;

        // A refusal caused by that teardown is not a verdict on the bundle
        // — the registration died. Report the dead sink instead, so the CLA
        // does not mistake its own unregistration for a per-bundle refusal.
        if matches!(verdict, Acceptance::Refused) && cla.cancel.is_cancelled() {
            return Err(Error::Disconnected);
        }
        Ok(verdict)
    }

    async fn add_peer(&self, cla_addr: Address, node_ids: &[NodeId]) -> Result<bool> {
        let cla = self.cla.upgrade().ok_or(Error::Disconnected)?;
        let registry = self.registry.upgrade().ok_or(Error::Disconnected)?;
        let dispatcher = self.dispatcher.upgrade().ok_or(Error::Disconnected)?;
        Ok(registry.add_peer(cla, dispatcher, cla_addr, node_ids).await)
    }

    async fn remove_peer(&self, cla_addr: &Address) -> Result<bool> {
        let cla = self.cla.upgrade().ok_or(Error::Disconnected)?;
        let registry = self.registry.upgrade().ok_or(Error::Disconnected)?;
        Ok(registry.remove_peer(cla, cla_addr).await)
    }

    async fn transfer_outcome(&self, bundle_id: &Id, outcome: TransferOutcome) -> Result<()> {
        let cla = self.cla.upgrade().ok_or(Error::Disconnected)?;
        let dispatcher = self.dispatcher.upgrade().ok_or(Error::Disconnected)?;
        dispatcher.transfer_outcome(&cla, bundle_id, outcome).await;
        Ok(())
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let (Some(cla), Some(registry)) = (self.cla.upgrade(), self.registry.upgrade()) {
            let tasks = registry.tasks.clone();
            hardy_async::spawn!(tasks, "cla_drop_cleanup", async move {
                registry.unregister(cla).await;
            });
        }
    }
}

// A CLA awaiting registration, held by the building-phase registry.
struct PendingCla {
    cla: Arc<dyn cla::Cla>,
    policy: Option<Arc<dyn FlowControllerFactory>>,
    init: ClaInit,
}

// The registry before the BPA starts: declarations only, no registration.
pub struct ClaRegistryBuilder {
    clas: HashMap<String, PendingCla>,
}

impl ClaRegistryBuilder {
    pub fn new() -> Self {
        Self {
            clas: Default::default(),
        }
    }

    pub fn insert(
        &mut self,
        name: String,
        cla: Arc<dyn cla::Cla>,
        policy: Option<Arc<dyn FlowControllerFactory>>,
        init: ClaInit,
    ) -> Result<()> {
        let Entry::Vacant(e) = self.clas.entry(name.clone()) else {
            return Err(Error::AlreadyExists(name));
        };
        info!("Inserted CLA: {name}");
        e.insert(PendingCla { cla, policy, init });
        Ok(())
    }

    // Transition to the running registry with the configured CLAs parked.
    // Activation happens in [`ClaRegistry::start`], after storage recovery:
    // `Cla::on_register` opens listeners (tcpclv4), and registrations must
    // not race the consistency check.
    pub fn build(
        self,
        node_ids: &Arc<NodeIds>,
        poll_channel_depth: usize,
        rib: &Arc<Rib>,
        store: &Arc<Store>,
    ) -> Arc<ClaRegistry> {
        let peers = Arc::new(PeerTable::new());
        Arc::new(ClaRegistry {
            node_ids: node_ids.clone(),
            clas: Mutex::new(Default::default()),
            pending: Mutex::new(self.clas.into_iter().collect()),
            rib: rib.clone(),
            store: store.clone(),
            peers,
            poll_channel_depth,
            tasks: TaskPool::new(),
        })
    }
}

// The registry once the BPA is running.
pub struct ClaRegistry {
    node_ids: Arc<NodeIds>,
    clas: Mutex<RecordMap>,
    // Builder-configured CLAs, parked until `start` activates them after
    // storage recovery.
    pending: Mutex<Vec<(String, PendingCla)>>,
    rib: Arc<Rib>,
    store: Arc<Store>,
    peers: Arc<PeerTable>,
    poll_channel_depth: usize,
    tasks: TaskPool,
}

impl ClaRegistry {
    /// Activate the builder-configured CLAs, in configuration order.
    /// Called by `Bpa::start` once storage recovery has completed: a CLA
    /// going live earlier would race the consistency check.
    pub async fn start(self: &Arc<Self>, dispatcher: &Arc<Dispatcher>) {
        let pending = core::mem::take(&mut *self.pending.lock());
        for (name, pending) in pending {
            // The builder pre-checked duplicate names; a failure here is a
            // bug, not a config error.
            if let Err(e) = self
                .register(
                    name.clone(),
                    pending.cla,
                    dispatcher,
                    pending.policy,
                    pending.init,
                )
                .await
            {
                error!("Failed to activate configured CLA {name}: {e}");
            }
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
        self.peers.forward(peer_id, next_hop, bundle).await
    }

    pub async fn shutdown(&self) {
        let clas = self.clas.lock().drain().map(|(_, v)| v).collect::<Vec<_>>();

        if !clas.is_empty() {
            metrics::gauge!("bpa.cla.registered").decrement(clas.len() as f64);
        }

        for cla in clas {
            self.unregister_cla(cla).await;
        }

        self.tasks.shutdown().await;
    }

    // Full registration in one step (for runtime dynamic registration via gRPC).
    pub async fn register(
        self: &Arc<Self>,
        name: String,
        cla: Arc<dyn cla::Cla>,
        dispatcher: &Arc<Dispatcher>,
        policy: Option<Arc<dyn FlowControllerFactory>>,
        init: ClaInit,
    ) -> Result<Vec<NodeId>> {
        // The effective cap: the BPA's configured max folded with whatever
        // limit the CLA declared at registration — the min of the caps
        // declared, or `None` when neither side has one.
        let max_bundle_size = match (dispatcher.max_bundle_size(), init.max_bundle_size) {
            (Some(bpa_cap), Some(cla_cap)) => Some(bpa_cap.min(cla_cap)),
            (bpa_cap, cla_cap) => bpa_cap.or(cla_cap),
        };

        let entry = {
            let mut clas = self.clas.lock();
            let Entry::Vacant(e) = clas.entry(name.clone()) else {
                return Err(Error::AlreadyExists(name));
            };
            e.insert(Arc::new(Record {
                cancel: CancellationToken::new(),
                cla,
                lane_count: init.lane_count,
                peers: Default::default(),
                name: Arc::from(name.as_str()),
                policy: policy
                    .unwrap_or_else(|| Arc::new(null_policy::FlowControllerFactory::new())),
            }))
            .clone()
        };

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();

        entry
            .cla
            .on_register(
                Box::new(Sink {
                    cla: Arc::downgrade(&entry),
                    registry: Arc::downgrade(self),
                    dispatcher: Arc::downgrade(dispatcher),
                }),
                &node_ids,
                max_bundle_size,
            )
            .await;

        metrics::gauge!("bpa.cla.registered").increment(1.0);
        info!("Registered CLA: {name}");

        Ok(node_ids)
    }

    async fn unregister(&self, cla: Arc<Record>) {
        let cla = self.clas.lock().remove(&*cla.name);

        if let Some(cla) = cla {
            metrics::gauge!("bpa.cla.registered").decrement(1.0);
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<Record>) {
        // First: wake every in-flight stream of this registration.
        cla.cancel.cancel();

        cla.cla.on_unregister().await;

        let peers = core::mem::take(&mut *cla.peers.lock());
        for (_, (node_ids, peer_id)) in peers {
            // Close the peer's egress queues first, then withdraw its RIB
            // entries: the ForwardPending/ForwardAckPending sweeps inside
            // remove_forward must run once no new send can re-enter the
            // queue (mirrors remove_peer). A send that lands after the
            // close bounces and is parked back to Waiting by its caller.
            self.peers.remove(peer_id).await;
            for node_id in node_ids {
                self.rib.remove_forward(node_id, peer_id).await;
                metrics::gauge!("bpa.fib.entries", "cla" => cla.name.clone()).decrement(1.0);
            }
        }

        info!("Unregistered CLA: {}", cla.name);
    }

    async fn add_peer(
        &self,
        cla: Arc<Record>,
        dispatcher: Arc<Dispatcher>,
        cla_addr: Address,
        node_ids: &[NodeId],
    ) -> bool {
        // Mint the id without publishing anything (reserved against reuse),
        // then claim the address — the adjacency's natural key — so a
        // duplicate exits before any peer state exists.
        let reservation = self.peers.reserve();
        let peer_id = reservation.id();
        let claimed = {
            let mut peers = cla.peers.lock();
            match peers.entry(cla_addr.clone()) {
                Entry::Vacant(e) => {
                    e.insert((node_ids.to_vec(), peer_id));
                    true
                }
                Entry::Occupied(_) => false,
            }
        };
        if !claimed {
            return false;
        }

        let cla_name = cla.name.clone();

        debug!("Added new peer {peer_id}: [{node_ids:?}] at {cla_addr} via CLA {cla_name}");

        // Construct the peer complete, then publish: the table only ever
        // holds working peers.
        let peer = Peer::start(
            self.poll_channel_depth,
            cla.clone(),
            peer_id,
            cla_addr.clone(),
            self.store.clone(),
            dispatcher,
            &self.tasks,
        )
        .await;
        reservation.publish(peer);

        // Post-construction liveness re-check: a concurrent
        // remove_peer/unregister_cla during construction has already taken
        // the address entry — withdraw the published peer instead of
        // installing RIB entries nothing will ever clean up.
        let still_ours = matches!(cla.peers.lock().get(&cla_addr), Some((_, id)) if *id == peer_id);
        if !still_ours {
            self.peers.remove(peer_id).await;
            return false;
        }

        // A neighbour — one whose node ids are not known — gets no RIB
        // entry, and nothing supplies them later.
        for node_id in node_ids {
            let _ = self.rib.add_forward(node_id.clone(), peer_id).await;
            metrics::gauge!("bpa.fib.entries", "cla" => cla_name.clone()).increment(1.0);
        }

        true
    }

    async fn remove_peer(&self, cla: Arc<Record>, cla_addr: &Address) -> bool {
        let Some((node_ids, peer_id)) = cla.peers.lock().remove(cla_addr) else {
            return false;
        };

        self.peers.remove(peer_id).await;
        for node_id in node_ids {
            self.rib.remove_forward(node_id, peer_id).await;
            metrics::gauge!("bpa.fib.entries", "cla" => cla.name.clone()).decrement(1.0);
        }

        debug!("Removed peer {peer_id}");

        true
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU64;

    use hardy_async::sync::spin::Once;
    use hardy_bpv7::eid::IpnNodeId;

    use super::*;
    use crate::{
        bpa::{Bpa, BpaRegistration},
        cla::ForwardOutcome,
    };

    struct TestCla {
        sink: Once<Box<dyn cla::Sink>>,
    }

    impl TestCla {
        fn new() -> Self {
            Self { sink: Once::new() }
        }
    }

    #[async_trait]
    impl cla::Cla for TestCla {
        async fn on_register(
            &self,
            sink: Box<dyn cla::Sink>,
            _node_ids: &[NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
            self.sink.call_once(|| sink);
        }
        async fn on_unregister(&self) {}
        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &Address,
            _bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<Segment>,
        ) -> Result<ForwardOutcome> {
            Ok(ForwardOutcome::Sent)
        }
    }

    // Registering a CLA with an already-in-use name should fail.
    #[tokio::test]
    async fn test_duplicate_registration() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;

        let cla1 = Arc::new(TestCla::new());
        let result = bpa
            .register_cla("test-cla".to_string(), cla1, None, ClaInit::default())
            .await;
        assert!(result.is_ok(), "First CLA registration should succeed");

        let cla2 = Arc::new(TestCla::new());
        let result = bpa
            .register_cla("test-cla".to_string(), cla2, None, ClaInit::default())
            .await;
        assert!(
            matches!(result, Err(Error::AlreadyExists(ref name)) if name == "test-cla"),
            "Duplicate CLA name should return AlreadyExists, got: {result:?}"
        );

        bpa.shutdown().await;
    }

    // Adding a peer installs a RIB entry; removing it withdraws it.
    #[tokio::test]
    async fn test_peer_lifecycle() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;

        let cla = Arc::new(TestCla::new());
        bpa.register_cla(
            "lifecycle-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .unwrap();

        let sink = cla.sink.get().expect("Sink should be set after register");
        let peer_addr = Address::Private("peer1".as_bytes().into());
        let peer_node = NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 10,
        });

        let added = sink
            .add_peer(peer_addr.clone(), core::slice::from_ref(&peer_node))
            .await
            .unwrap();
        assert!(added, "First add_peer should succeed");

        let removed = sink.remove_peer(&peer_addr).await.unwrap();
        assert!(removed, "remove_peer should succeed");

        let removed = sink.remove_peer(&peer_addr).await.unwrap();
        assert!(!removed, "Double remove_peer should return false");

        bpa.shutdown().await;
    }

    // Unregistering a CLA should remove all its peers.
    #[tokio::test]
    async fn test_cascading_cleanup() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;

        let cla = Arc::new(TestCla::new());
        bpa.register_cla(
            "cascade-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .unwrap();

        let sink = cla.sink.get().expect("Sink should be set");

        let addr1 = Address::Private("p1".as_bytes().into());
        let addr2 = Address::Private("p2".as_bytes().into());
        let node1 = NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 20,
        });
        let node2 = NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 21,
        });

        sink.add_peer(addr1, &[node1]).await.unwrap();
        sink.add_peer(addr2, &[node2]).await.unwrap();

        sink.unregister().await;

        let cla2 = Arc::new(TestCla::new());
        let result = bpa
            .register_cla("cascade-cla".to_string(), cla2, None, ClaInit::default())
            .await;
        assert!(
            result.is_ok(),
            "Re-registration after unregister should succeed, got: {result:?}"
        );

        bpa.shutdown().await;
    }
}
