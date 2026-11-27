use core::num::NonZeroU32;

use hardy_async::sync::spin::Once;
use hardy_bpv7::eid::{Eid, NodeId};

use self::sink::{Sink, SinkHandle};
use super::{
    factory::{BoxFactory, Context, Terms},
    *,
};

mod sink;

// CLA registry uses hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. No read-only access pattern (RwLock not needed)
// 3. No blocking/RNG/iteration while holding lock
// 4. Avoids OS mutex overhead on CLA lifecycle operations

pub struct Cla {
    pub cla: Arc<dyn cla::Cla>,
    pub policy: Arc<dyn policy::FlowControllerFactory>,
    // Snapshotted from the factory's `ClaInit` declarations at registration.
    pub address_type: Option<ClaAddressType>,
    pub lane_count: Option<NonZeroU32>,

    name: Arc<str>,
    // Cancelled at unregistration; every in-flight stream of this
    // registration races it and fails immediately.
    cancel: hardy_async::CancellationToken,
    // sync::spin::Mutex for O(1) peer HashMap operations
    // Key: ClaAddress (primary key for a link-layer adjacency)
    // Value: (known EIDs for the peer, peer_id in PeerTable)
    // An empty EID vec means a Neighbour (EID not yet known; no RIB entry installed)
    peers: hardy_async::sync::spin::Mutex<HashMap<ClaAddress, (Vec<NodeId>, u32)>>,
}

impl PartialEq for Cla {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Cla {}

impl PartialOrd for Cla {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cla {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl core::hash::Hash for Cla {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl core::fmt::Debug for Cla {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cla")
            .field("name", &self.name)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}

impl Cla {
    // Whether `peer_id` is one of this CLA's peers — the ownership check for
    // deferred transfer outcomes.
    pub fn owns_peer(&self, peer_id: u32) -> bool {
        self.peers.lock().values().any(|(_, id)| *id == peer_id)
    }
}

type ClaMap = HashMap<String, Arc<Cla>>;

// A configured CLA, built when the registry starts.
struct PendingCla {
    name: String,
    factory: BoxFactory,
    policy: Option<Arc<dyn policy::FlowControllerFactory>>,
}

// CLA registry in the building phase — only insert() is available.
pub struct ClaRegistryBuilder {
    clas: Vec<PendingCla>,
}

impl ClaRegistryBuilder {
    pub fn new() -> Self {
        Self { clas: Vec::new() }
    }

    // Parks a configured CLA, in configuration order, refusing a name
    // already configured.
    pub fn insert(
        &mut self,
        name: String,
        factory: BoxFactory,
        policy: Option<Arc<dyn policy::FlowControllerFactory>>,
    ) -> cla::Result<()> {
        if self.clas.iter().any(|pending| pending.name == name) {
            return Err(cla::Error::AlreadyExists(name));
        }
        info!("Inserted CLA: {name}");
        self.clas.push(PendingCla {
            name,
            factory,
            policy,
        });
        Ok(())
    }

    // Transition to the running registry with the configured CLAs parked.
    // Building and activation happen in [`ClaRegistry::start`], after
    // storage recovery: `Cla::on_register` opens listeners (tcpclv4), and
    // registrations must not race the consistency check.
    pub fn build(
        self,
        node_ids: &Arc<node_ids::NodeIds>,
        poll_channel_depth: usize,
        rib: &Arc<routing::Rib>,
        store: &Arc<storage::store::Store>,
    ) -> Arc<ClaRegistry> {
        let peers = Arc::new(cla::peers::PeerTable::new());
        Arc::new(ClaRegistry {
            node_ids: node_ids.clone(),
            clas: hardy_async::sync::spin::Mutex::new(Default::default()),
            pending: hardy_async::sync::spin::Mutex::new(self.clas),
            rib: rib.clone(),
            store: store.clone(),
            peers,
            poll_channel_depth,
            tasks: hardy_async::TaskPool::new(),
        })
    }
}

// CLA registry in the running phase — full register/unregister available.
pub struct ClaRegistry {
    node_ids: Arc<node_ids::NodeIds>,
    clas: hardy_async::sync::spin::Mutex<ClaMap>,
    // Builder-configured CLAs, parked until `start` builds and activates
    // them after storage recovery.
    pending: hardy_async::sync::spin::Mutex<Vec<PendingCla>>,
    rib: Arc<routing::Rib>,
    store: Arc<storage::store::Store>,
    peers: Arc<peers::PeerTable>,
    poll_channel_depth: usize,
    tasks: hardy_async::TaskPool,
}

impl ClaRegistry {
    /// Build and activate the builder-configured CLAs, in configuration
    /// order. Called by `Bpa::start` once storage recovery has completed: a
    /// CLA going live earlier would race the consistency check.
    pub async fn start(self: &Arc<Self>, dispatcher: &Arc<dispatcher::Dispatcher>) {
        let pending = core::mem::take(&mut *self.pending.lock());
        for PendingCla {
            name,
            factory,
            policy,
        } in pending
        {
            // The builder refused duplicate names, so a failure here is a
            // factory refusing its terms.
            if let Err(e) = self
                .register(name.clone(), factory, dispatcher, policy)
                .await
            {
                error!("Failed to activate configured CLA {name}: {e}");
            }
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

    // Full registration in one step: builds the CLA against the settled
    // terms, files it under `name`, and activates it. Configured CLAs and
    // runtime registrations (in-process or via gRPC) all arrive here.
    //
    // The build is wiring only, so the name is checked once the CLA exists:
    // a duplicate drops a CLA that was never filed or started.
    pub async fn register(
        self: &Arc<Self>,
        name: String,
        factory: BoxFactory,
        dispatcher: &Arc<dispatcher::Dispatcher>,
        policy: Option<Arc<dyn policy::FlowControllerFactory>>,
    ) -> cla::Result<Vec<NodeId>> {
        let address_type = factory.init.address_type;
        let terms = Terms::negotiate(&factory.init, dispatcher.max_bundle_size());
        let sink = Arc::new(Sink {
            cla: Once::new(),
            registry: self.clone(),
            dispatcher: dispatcher.clone(),
        });
        let cla = factory.build(Context::new(sink.clone(), self.node_ids.clone(), terms))?;

        let entry = self.file(name.clone(), cla, address_type, terms.lane_count, policy)?;
        sink.cla.call_once(|| Arc::downgrade(&entry));

        // Register that the CLA is a handler for the address type
        if let Some(address_type) = entry.address_type {
            self.rib.add_address_type(address_type, entry.clone());
        }

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();

        entry
            .cla
            .on_register(Box::new(SinkHandle(sink)), &node_ids, terms.max_bundle_size)
            .await;

        metrics::gauge!("bpa.cla.registered").increment(1.0);
        info!("Registered CLA: {name}");

        Ok(node_ids)
    }

    // Files a built CLA under `name`, the only way one becomes reachable.
    fn file(
        &self,
        name: String,
        cla: Arc<dyn cla::Cla>,
        address_type: Option<ClaAddressType>,
        lane_count: Option<NonZeroU32>,
        policy: Option<Arc<dyn policy::FlowControllerFactory>>,
    ) -> cla::Result<Arc<Cla>> {
        let mut clas = self.clas.lock();
        let hash_map::Entry::Vacant(e) = clas.entry(name.clone()) else {
            return Err(cla::Error::AlreadyExists(name));
        };
        Ok(e.insert(Arc::new(Cla {
            cancel: hardy_async::CancellationToken::new(),
            cla,
            address_type,
            lane_count,
            peers: Default::default(),
            name: Arc::from(name.as_str()),
            policy: policy
                .unwrap_or_else(|| Arc::new(policy::null_policy::FlowControllerFactory::new())),
        }))
        .clone())
    }

    async fn unregister(&self, cla: Arc<Cla>) {
        let cla = self.clas.lock().remove(&*cla.name);

        if let Some(cla) = cla {
            metrics::gauge!("bpa.cla.registered").decrement(1.0);
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<Cla>) {
        // First: wake every in-flight stream of this registration.
        cla.cancel.cancel();

        cla.cla.on_unregister().await;

        if let Some(address_type) = cla.address_type {
            self.rib.remove_address_type(&address_type);
        }

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
        cla: Arc<Cla>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cla_addr: ClaAddress,
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
                hash_map::Entry::Vacant(e) => {
                    e.insert((node_ids.to_vec(), peer_id));
                    true
                }
                hash_map::Entry::Occupied(_) => false,
            }
        };
        if !claimed {
            return false;
        }

        let cla_name = cla.name.clone();

        debug!("Added new peer {peer_id}: [{node_ids:?}] at {cla_addr} via CLA {cla_name}");

        // Construct the peer complete, then publish: the table only ever
        // holds working peers.
        let peer = peers::Peer::start(
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

        // Add RIB entry for each known EID.
        // Neighbours (empty node_ids) get no RIB entry; nothing yet supplies
        // their EIDs later.
        for node_id in node_ids {
            let _ = self.rib.add_forward(node_id.clone(), peer_id).await;
            metrics::gauge!("bpa.fib.entries", "cla" => cla_name.clone()).increment(1.0);
        }

        true
    }

    async fn remove_peer(&self, cla: Arc<Cla>, cla_addr: &ClaAddress) -> bool {
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
    use super::*;
    use crate::bpa::{Bpa, BpaRegistration};
    use hardy_async::sync::spin::Once;

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
            _node_ids: &[hardy_bpv7::eid::NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
            self.sink.call_once(|| sink);
        }
        async fn on_unregister(&self) {}
        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &ClaAddress,
            _bundle_id: &hardy_bpv7::bundle::Id,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<Segment>,
        ) -> cla::Result<cla::ForwardBundleResult> {
            Ok(cla::ForwardBundleResult::Sent)
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
            matches!(result, Err(cla::Error::AlreadyExists(ref name)) if name == "test-cla"),
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
        let peer_addr = ClaAddress::Private("peer1".as_bytes().into());
        let peer_node = hardy_bpv7::eid::NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
            allocator_id: 0,
            node_number: 10,
        });

        // Add peer
        let added = sink
            .add_peer(peer_addr.clone(), core::slice::from_ref(&peer_node))
            .await
            .unwrap();
        assert!(added, "First add_peer should succeed");

        // Remove peer
        let removed = sink.remove_peer(&peer_addr).await.unwrap();
        assert!(removed, "remove_peer should succeed");

        // Removing again should return false
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

        // Add two peers
        let addr1 = ClaAddress::Private("p1".as_bytes().into());
        let addr2 = ClaAddress::Private("p2".as_bytes().into());
        let node1 = hardy_bpv7::eid::NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
            allocator_id: 0,
            node_number: 20,
        });
        let node2 = hardy_bpv7::eid::NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
            allocator_id: 0,
            node_number: 21,
        });

        sink.add_peer(addr1, &[node1]).await.unwrap();
        sink.add_peer(addr2, &[node2]).await.unwrap();

        // Unregister the CLA — should cascade-remove both peers
        sink.unregister().await;

        // Re-registering with same name should now succeed (name freed)
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
