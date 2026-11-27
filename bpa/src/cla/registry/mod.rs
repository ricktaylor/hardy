use core::num::NonZeroU32;

use hardy_async::sync::spin::Once;
use hardy_bpv7::eid::{Eid, NodeId};

pub use self::record::Cla;
use self::sink::{Sink, SinkHandle};
use super::{
    factory::{BoxFactory, Context, Terms},
    *,
};
use crate::claim_table::ClaimTable;

mod record;
mod sink;

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
            clas: ClaimTable::new(),
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
    clas: ClaimTable<String, Arc<Cla>>,
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
            // factory refusing its terms, or a name a runtime registration
            // claimed first.
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
        let clas = self.clas.drain();

        if !clas.is_empty() {
            metrics::gauge!("bpa.cla.registered").decrement(clas.len() as f64);
        }

        for (_, cla) in clas {
            self.unregister_cla(cla).await;
        }

        self.tasks.shutdown().await;
    }

    // Full registration in one step: claims `name`, builds the CLA against
    // the settled terms, files it under the claim, and activates it.
    // Configured CLAs and runtime registrations (in-process or via gRPC)
    // all arrive here.
    //
    // The name is claimed before the factory runs, so a taken name is
    // refused before anything is built, and the factory runs outside the
    // registry's lock.
    pub async fn register(
        self: &Arc<Self>,
        name: String,
        factory: BoxFactory,
        dispatcher: &Arc<dispatcher::Dispatcher>,
        policy: Option<Arc<dyn policy::FlowControllerFactory>>,
    ) -> cla::Result<Vec<NodeId>> {
        if !self.clas.claim(name.clone()) {
            return Err(cla::Error::AlreadyExists(name));
        }
        let address_type = factory.init.address_type;
        let terms = Terms::negotiate(&factory.init, dispatcher.max_bundle_size());
        let sink = Arc::new(Sink {
            cla: Once::new(),
            registry: self.clone(),
            dispatcher: dispatcher.clone(),
        });
        let cla = match factory.build(Context::new(sink.clone(), self.node_ids.clone(), terms)) {
            Ok(cla) => cla,
            Err(e) => {
                self.clas.release(&name);
                return Err(e);
            }
        };

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

    // Files a built CLA against the claim on `name`, the only way one
    // becomes reachable. A shutdown that ran while the factory did took the
    // claim with it; the CLA was never reached, so dropping it is the whole
    // teardown.
    fn file(
        &self,
        name: String,
        cla: Arc<dyn cla::Cla>,
        address_type: Option<ClaAddressType>,
        lane_count: Option<NonZeroU32>,
        policy: Option<Arc<dyn policy::FlowControllerFactory>>,
    ) -> cla::Result<Arc<Cla>> {
        let entry = Arc::new(Cla {
            cancel: hardy_async::CancellationToken::new(),
            cla,
            address_type,
            lane_count,
            peers: Default::default(),
            name: Arc::from(name.as_str()),
            policy: policy
                .unwrap_or_else(|| Arc::new(policy::null_policy::FlowControllerFactory::new())),
        });
        if !self.clas.file(&name, entry.clone()) {
            return Err(cla::Error::Disconnected);
        }
        Ok(entry)
    }

    async fn unregister(&self, cla: Arc<Cla>) {
        // Only this registration: a stale sink must not take a later
        // registration under the same name.
        let cla = self
            .clas
            .remove_if(&*cla.name, |filed| Arc::ptr_eq(filed, &cla));

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

    // Sends what the registry says about the CLA's name while the factory
    // runs: `Some(true)` is a claim seen through a free lock.
    struct ProbeFactory {
        registry: Weak<ClaRegistry>,
        name: &'static str,
        seen_tx: flume::Sender<Option<bool>>,
    }

    impl Factory for ProbeFactory {
        fn build(self, _cx: Context) -> cla::Result<Arc<dyn cla::Cla>> {
            let registry = self
                .registry
                .upgrade()
                .expect("the registry outlives the build");
            let _ = self.seen_tx.send(registry.clas.is_building(self.name));
            Ok(Arc::new(TestCla::new()))
        }
    }

    fn probe(
        registry: &Arc<ClaRegistry>,
        name: &'static str,
    ) -> (BoxFactory, flume::Receiver<Option<bool>>) {
        let (seen_tx, seen_rx) = flume::bounded(1);
        let factory = BoxFactory::new(ProbeFactory {
            registry: Arc::downgrade(registry),
            name,
            seen_tx,
        });
        (factory, seen_rx)
    }

    // The factory runs against a claim on its name, outside the registry's
    // lock: a factory may log, allocate or reach back into the registry,
    // any of which would spin against a lock still held.
    #[tokio::test]
    async fn the_factory_runs_against_a_claim_outside_the_lock() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;
        let (registry, dispatcher) = bpa.cla_registry();

        let (factory, seen_rx) = probe(registry, "probe-cla");
        registry
            .register("probe-cla".into(), factory, dispatcher, None)
            .await
            .expect("registration should succeed");
        assert_eq!(
            seen_rx.try_recv().unwrap(),
            Some(true),
            "the factory must see its name claimed (Some(true)), not the lock held (None) or the name free (Some(false))"
        );

        bpa.shutdown().await;
    }

    // A name already registered is refused before its factory runs.
    #[tokio::test]
    async fn a_taken_name_is_refused_before_the_build() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;
        bpa.register_cla(
            "taken".to_string(),
            Arc::new(TestCla::new()),
            None,
            ClaInit::default(),
        )
        .await
        .unwrap();
        let (registry, dispatcher) = bpa.cla_registry();

        let (factory, seen_rx) = probe(registry, "taken");
        let result = registry
            .register("taken".into(), factory, dispatcher, None)
            .await;
        assert!(
            matches!(result, Err(cla::Error::AlreadyExists(ref name)) if name == "taken"),
            "{result:?}"
        );
        assert!(seen_rx.is_empty(), "the factory must never run");

        bpa.shutdown().await;
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
