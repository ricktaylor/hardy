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

    name: String,
    // sync::spin::Mutex for O(1) peer HashMap operations
    // Key: ClaAddress (primary key for a link-layer adjacency)
    // Value: (known EIDs for the peer, peer_id in PeerTable)
    // An empty EID vec means a Neighbour (EID not yet known; no RIB entry installed)
    peers: hardy_async::sync::spin::Mutex<HashMap<ClaAddress, (Vec<NodeId>, u32)>>,
    address_type: Option<ClaAddressType>,
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
            .field("address_type", &self.address_type)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}

type ClaMap = HashMap<String, Arc<Cla>>;

struct Sink {
    cla: Weak<Cla>,
    registry: Arc<ClaRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

#[async_trait]
impl cla::Sink for Sink {
    async fn unregister(&self) {
        if let Some(cla) = self.cla.upgrade() {
            self.registry.unregister(cla).await;
        }
    }

    async fn dispatch(
        &self,
        bundle: Bytes,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&ClaAddress>,
    ) -> cla::Result<()> {
        let cla_name = self.cla.upgrade().map(|c| c.name.clone().into());
        self.dispatcher
            .receive_bundle(bundle, cla_name, peer_node.cloned(), peer_addr.cloned())
            .await
    }

    async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> cla::Result<bool> {
        let cla = self.cla.upgrade().ok_or(cla::Error::Disconnected)?;
        Ok(self
            .registry
            .add_peer(cla, self.dispatcher.clone(), cla_addr, node_ids)
            .await)
    }

    async fn remove_peer(&self, cla_addr: &ClaAddress) -> cla::Result<bool> {
        Ok(self
            .registry
            .remove_peer(
                self.cla.upgrade().ok_or(cla::Error::Disconnected)?,
                cla_addr,
            )
            .await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(cla) = self.cla.upgrade() {
            let registry = self.registry.clone();
            hardy_async::spawn!(self.registry.tasks, "cla_drop_cleanup", async move {
                registry.unregister(cla).await;
            });
        }
    }
}

/// CLA registry in the building phase — only insert() is available.
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
        address_type: Option<ClaAddressType>,
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
            name,
            address_type,
            policy: policy.unwrap_or_else(|| Arc::new(policy::null_policy::EgressPolicy::new())),
        }));
        Ok(())
    }

    /// Transition to the running registry by registering all inserted CLAs.
    pub async fn build(
        self,
        node_ids: &Arc<node_ids::NodeIds>,
        poll_channel_depth: usize,
        rib: &Arc<rib::Rib>,
        store: &Arc<storage::Store>,
        peers: Arc<peers::PeerTable>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> cla::Result<Arc<ClaRegistry>> {
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
                    cla.name.clone(),
                    cla.address_type,
                    cla.cla.clone(),
                    dispatcher,
                    Some(cla.policy.clone()),
                )
                .await?;
        }

        Ok(registry)
    }
}

/// CLA registry in the running phase — full register/unregister available.
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

    /// Full registration in one step (for runtime dynamic registration via gRPC).
    pub async fn register(
        self: &Arc<Self>,
        name: String,
        address_type: Option<ClaAddressType>,
        cla: Arc<dyn cla::Cla>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
        policy: Option<Arc<dyn policy::EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        {
            let mut clas = self.clas.lock();
            let hash_map::Entry::Vacant(e) = clas.entry(name.clone()) else {
                return Err(cla::Error::AlreadyExists(name));
            };
            e.insert(Arc::new(Cla {
                cla,
                peers: Default::default(),
                name: name.clone(),
                address_type,
                policy: policy
                    .unwrap_or_else(|| Arc::new(policy::null_policy::EgressPolicy::new())),
            }));
        }

        let cla = self.clas.lock().get(&name).cloned().unwrap();

        if let Some(address_type) = address_type {
            self.rib.add_address_type(address_type, cla.clone());
        }

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();
        cla.cla
            .on_register(
                Box::new(Sink {
                    cla: Arc::downgrade(&cla),
                    registry: self.clone(),
                    dispatcher: dispatcher.clone(),
                }),
                &node_ids,
            )
            .await;

        metrics::gauge!("bpa.cla.registered").increment(1.0);
        info!("Registered CLA: {name}");

        Ok(node_ids)
    }

    async fn unregister(&self, cla: Arc<Cla>) {
        let cla = self.clas.lock().remove(&cla.name);

        if let Some(cla) = cla {
            metrics::gauge!("bpa.cla.registered").decrement(1.0);
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<Cla>) {
        cla.cla.on_unregister().await;

        if let Some(address_type) = &cla.address_type {
            self.rib.remove_address_type(address_type);
        }

        let peers = core::mem::take(&mut *cla.peers.lock());

        for (_, (node_ids, peer_id)) in peers {
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
        ) {
            self.sink.call_once(|| sink);
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
        let result = bpa
            .register_cla("test-cla".to_string(), None, cla1, None)
            .await;
        assert!(result.is_ok(), "First CLA registration should succeed");

        let cla2 = Arc::new(TestCla::new());
        let result = bpa
            .register_cla("test-cla".to_string(), None, cla2, None)
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
        bpa.start(false);

        let cla = Arc::new(TestCla::new());
        bpa.register_cla("lifecycle-cla".to_string(), None, cla.clone(), None)
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
            .add_peer(peer_addr.clone(), std::slice::from_ref(&peer_node))
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
        bpa.start(false);

        let cla = Arc::new(TestCla::new());
        bpa.register_cla("cascade-cla".to_string(), None, cla.clone(), None)
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
            .register_cla("cascade-cla".to_string(), None, cla2, None)
            .await;
        assert!(
            result.is_ok(),
            "Re-registration after unregister should succeed, got: {result:?}"
        );

        bpa.shutdown().await;
    }
}
