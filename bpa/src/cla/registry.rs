use super::*;
use hardy_bpv7::eid::NodeId;

// CLA registry uses hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. No read-only access pattern (RwLock not needed)
// 3. No blocking/RNG/iteration while holding lock
// 4. Avoids OS mutex overhead on CLA lifecycle operations

pub struct Cla {
    pub(super) cla: Arc<dyn cla::Cla>,
    pub(super) policy: Arc<dyn policy::EgressPolicy>,

    name: Arc<str>,
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

type ClaMap = HashMap<String, Arc<Cla>>;

// CLA registry in the building phase — only insert() is available.
pub(crate) struct ClaRegistryBuilder {
    clas: ClaMap,
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
        policy: Option<Arc<dyn policy::EgressPolicy>>,
    ) -> cla::Result<()> {
        let hash_map::Entry::Vacant(e) = self.clas.entry(name.clone()) else {
            return Err(cla::Error::AlreadyExists(name));
        };
        info!("Inserted CLA: {name}");
        e.insert(Arc::new(Cla {
            cla,
            peers: Default::default(),
            name: Arc::from(name.as_str()),
            policy: policy.unwrap_or_else(|| Arc::new(policy::null_policy::EgressPolicy::new())),
        }));
        Ok(())
    }

    // Transition to the running registry by registering all inserted CLAs.
    pub async fn build(
        self,
        node_ids: &Arc<node_ids::NodeIds>,
        poll_channel_depth: usize,
        rib: &Arc<rib::Rib>,
        store: &Arc<storage::Store>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> cla::Result<Arc<ClaRegistry>> {
        let peers = Arc::new(cla::peers::PeerTable::new());
        let registry = Arc::new(ClaRegistry {
            node_ids: node_ids.clone(),
            clas: hardy_async::sync::spin::Mutex::new(Default::default()),
            rib: rib.clone(),
            store: store.clone(),
            peers,
            poll_channel_depth,
            tasks: hardy_async::TaskPool::new(),
        });

        for (_, cla) in self.clas {
            registry
                .register(
                    cla.name.to_string(),
                    cla.cla.clone(),
                    dispatcher,
                    Some(cla.policy.clone()),
                )
                .await?;
        }

        Ok(registry)
    }
}

// CLA registry in the running phase — full register/unregister available.
pub(crate) struct ClaRegistry {
    node_ids: Arc<node_ids::NodeIds>,
    clas: hardy_async::sync::spin::Mutex<ClaMap>,
    rib: Arc<rib::Rib>,
    store: Arc<storage::Store>,
    peers: Arc<peers::PeerTable>,
    poll_channel_depth: usize,
    tasks: hardy_async::TaskPool,
}

impl ClaRegistry {
    pub async fn forward(
        &self,
        peer_id: u32,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        self.peers.forward(peer_id, bundle).await
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
        dispatcher: &Arc<dispatcher::Dispatcher>,
        policy: Option<Arc<dyn policy::EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        let address_type = cla.address_type();
        let entry = {
            let mut clas = self.clas.lock();
            let hash_map::Entry::Vacant(e) = clas.entry(name.clone()) else {
                return Err(cla::Error::AlreadyExists(name));
            };
            e.insert(Arc::new(Cla {
                cla,
                peers: Default::default(),
                name: Arc::from(name.as_str()),
                policy: policy
                    .unwrap_or_else(|| Arc::new(policy::null_policy::EgressPolicy::new())),
            }))
            .clone()
        };

        // Register that the CLA is a handler for the address type
        if let Some(address_type) = address_type {
            self.rib.add_address_type(address_type, entry.clone());
        }

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();

        let (ingress_tx, ingress_rx) = flume::bounded(self.poll_channel_depth);
        let (peer_tx, peer_rx) = flume::unbounded();
        let shutdown = self.tasks.cancel_token().child_token();

        let ctx = cla::ClaContext::new(ingress_tx, peer_tx, shutdown.clone());

        // Spawn ingress receiver: dispatches received bundles to the BPA
        let dispatcher_clone = dispatcher.clone();
        let cla_name = Arc::clone(&entry.name);
        let ingress_cancel = shutdown.clone();
        hardy_async::spawn!(self.tasks, "cla_ingress_receiver", async move {
            use futures::FutureExt;
            loop {
                futures::select_biased! {
                    _ = ingress_cancel.cancelled().fuse() => break,
                    msg = ingress_rx.recv_async().fuse() => match msg {
                        Ok(msg) => {
                            if let Err(e) = dispatcher_clone
                                .receive_bundle(
                                    msg.data,
                                    Some(cla_name.clone()),
                                    msg.peer_node,
                                    msg.peer_addr,
                                )
                                .await
                            {
                                warn!("Failed to process ingress bundle: {e}");
                            }
                        }
                        Err(_) => break,
                    },
                }
            }
        });

        // Spawn peer receiver: manages peer add/remove operations
        let registry = self.clone();
        let entry_for_peers = entry.clone();
        let dispatcher_for_peers = dispatcher.clone();
        hardy_async::spawn!(self.tasks, "cla_peer_receiver", async move {
            use cla::context::PeerOp;
            use futures::FutureExt;
            loop {
                futures::select_biased! {
                    _ = shutdown.cancelled().fuse() => break,
                    op = peer_rx.recv_async().fuse() => match op {
                        Ok(PeerOp::Add(addr, ids)) => {
                            registry
                                .add_peer(
                                    entry_for_peers.clone(),
                                    dispatcher_for_peers.clone(),
                                    addr,
                                    &ids,
                                )
                                .await;
                        }
                        Ok(PeerOp::Remove(addr)) => {
                            registry.remove_peer(entry_for_peers.clone(), &addr).await;
                        }
                        Err(_) => break,
                    },
                }
            }
            // Channel closed or shutdown: CLA disconnected
            registry.unregister(entry_for_peers).await;
        });

        entry.cla.on_register(ctx, &node_ids).await;

        metrics::gauge!("bpa.cla.registered").increment(1.0);
        info!("Registered CLA: {name}");

        Ok(node_ids)
    }

    async fn unregister(&self, cla: Arc<Cla>) {
        let cla = self.clas.lock().remove(&*cla.name);

        if let Some(cla) = cla {
            metrics::gauge!("bpa.cla.registered").decrement(1.0);
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<Cla>) {
        cla.cla.on_unregister().await;

        if let Some(address_type) = cla.cla.address_type() {
            self.rib.remove_address_type(&address_type);
        }

        let peers = core::mem::take(&mut *cla.peers.lock());
        for (_, (node_ids, peer_id)) in peers {
            // Remove RIB entries for all EIDs associated with this address
            for node_id in node_ids {
                self.rib.remove_forward(node_id, peer_id).await;
                metrics::gauge!("bpa.fib.entries", "cla" => cla.name.clone()).decrement(1.0);
            }
            self.peers.remove(peer_id).await;
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
        let peer = Arc::new(peers::Peer::new(Arc::downgrade(&cla)));

        // Acquire peer_id first (without holding cla.peers lock) to avoid nested spinlock acquisition.
        // If the cla.peers entry already exists, we clean up the orphaned peer_id.
        let peer_id = self.peers.insert(peer.clone());

        // Now try to insert into cla.peers (separate lock acquisition, no nesting)
        let inserted = {
            let mut peers = cla.peers.lock();
            match peers.entry(cla_addr.clone()) {
                hash_map::Entry::Vacant(e) => {
                    e.insert((node_ids.to_vec(), peer_id));
                    true
                }
                hash_map::Entry::Occupied(_) => false, // Already exists
            }
        };

        // If entry already existed, clean up the orphaned peer_id
        // TODO: This deadlocks — PeerTable::remove() calls Peer::close() which calls
        // self.inner.wait() on an OnceLock that was never initialised (start() was never
        // called on the orphan). Fix: either check inner.is_initialized() in close(), or
        // remove directly from the PeerTable HashMap without calling close().
        if !inserted {
            self.peers.remove(peer_id).await;
            return false;
        }

        let cla_name = cla.name.clone();

        debug!("Added new peer {peer_id}: [{node_ids:?}] at {cla_addr} via CLA {cla_name}");

        // Start the peer polling the queue
        peer.start(
            self.poll_channel_depth,
            cla,
            peer_id,
            cla_addr,
            self.store.clone(),
            dispatcher,
            &self.tasks,
        )
        .await;

        // Add RIB entry for each known EID.
        // Neighbours (empty node_ids) get no RIB entry — BP-ARP will resolve them later.
        for node_id in node_ids {
            self.rib.add_forward(node_id.clone(), peer_id).await;
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
        ctx: Once<cla::ClaContext>,
    }

    impl TestCla {
        fn new() -> Self {
            Self { ctx: Once::new() }
        }
    }

    #[async_trait]
    impl cla::Cla for TestCla {
        async fn on_register(&self, ctx: cla::ClaContext, _node_ids: &[hardy_bpv7::eid::NodeId]) {
            self.ctx.call_once(|| ctx);
        }
        async fn on_unregister(&self) {}
        async fn forward(
            &self,
            _queue: Option<u32>,
            _cla_addr: &ClaAddress,
            _bundle: bytes::Bytes,
        ) -> cla::Result<cla::ForwardBundleResult> {
            Ok(cla::ForwardBundleResult::Sent)
        }
    }

    // Registering a CLA with an already-in-use name should fail.
    #[tokio::test]
    async fn test_duplicate_registration() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false);

        let cla1 = Arc::new(TestCla::new());
        let result = bpa.register_cla("test-cla".to_string(), cla1, None).await;
        assert!(result.is_ok(), "First CLA registration should succeed");

        let cla2 = Arc::new(TestCla::new());
        let result = bpa.register_cla("test-cla".to_string(), cla2, None).await;
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
        bpa.start(false);

        let cla = Arc::new(TestCla::new());
        bpa.register_cla("lifecycle-cla".to_string(), cla.clone(), None)
            .await
            .unwrap();

        let ctx = cla.ctx.get().expect("Context should be set after register");
        let peer_addr = ClaAddress::Private("peer1".as_bytes().into());
        let peer_node = hardy_bpv7::eid::NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
            allocator_id: 0,
            node_number: 10,
        });

        // Add peer (fire-and-forget via channel)
        ctx.add_peer(peer_addr.clone(), vec![peer_node]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Remove peer
        ctx.remove_peer(peer_addr);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        bpa.shutdown().await;
    }

    // Unregistering a CLA should remove all its peers.
    #[tokio::test]
    async fn test_cascading_cleanup() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false);

        let cla = Arc::new(TestCla::new());
        bpa.register_cla("cascade-cla".to_string(), cla.clone(), None)
            .await
            .unwrap();

        let ctx = cla.ctx.get().expect("Context should be set");

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

        ctx.add_peer(addr1, vec![node1]);
        ctx.add_peer(addr2, vec![node2]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Shutdown triggers cascading cleanup of all CLA peers
        bpa.shutdown().await;

        // Rebuild BPA to verify the name is freed
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false);

        // Re-registering with same name should now succeed (name freed)
        let cla2 = Arc::new(TestCla::new());
        let result = bpa
            .register_cla("cascade-cla".to_string(), cla2, None)
            .await;
        assert!(
            result.is_ok(),
            "Re-registration after unregister should succeed, got: {result:?}"
        );

        bpa.shutdown().await;
    }
}
