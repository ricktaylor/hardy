use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use hardy_bpv7::eid::NodeId;
use tracing::{debug, info};

use super::peers::{Peer, PeerTable};
use super::sink::ClaSink;
use crate::bundle::Bundle;
use crate::cla::{self, Cla, ClaAddress};
use crate::dispatcher::Dispatcher;
use crate::node_ids::NodeIds;
use crate::policy::{self, EgressPolicy};
use crate::rib::Rib;
use crate::storage::Store;
use crate::{HashMap, hash_map};

// CLA registry uses hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. No read-only access pattern (RwLock not needed)
// 3. No blocking/RNG/iteration while holding lock
// 4. Avoids OS mutex overhead on CLA lifecycle operations

pub struct ClaEntry {
    pub(super) cla: Arc<dyn Cla>,
    pub(super) policy: Arc<dyn EgressPolicy>,

    pub(super) name: Arc<str>,
    // sync::spin::Mutex for O(1) peer HashMap operations
    // Key: ClaAddress (primary key for a link-layer adjacency)
    // Value: (known EIDs for the peer, peer_id in PeerTable)
    // An empty EID vec means a Neighbour (EID not yet known; no RIB entry installed)
    pub(super) peers: hardy_async::sync::spin::Mutex<HashMap<ClaAddress, (Vec<NodeId>, u32)>>,
}

impl PartialEq for ClaEntry {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for ClaEntry {}

impl PartialOrd for ClaEntry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ClaEntry {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl core::hash::Hash for ClaEntry {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl core::fmt::Debug for ClaEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cla")
            .field("name", &self.name)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}

pub(crate) struct ClaRegistry {
    node_ids: Arc<NodeIds>,
    clas: hardy_async::sync::spin::Mutex<HashMap<String, Arc<ClaEntry>>>,
    rib: Arc<Rib>,
    store: Arc<Store>,
    peers: Arc<PeerTable>,
    poll_channel_depth: usize,
    tasks: hardy_async::TaskPool,
    drop_tx: flume::Sender<Weak<ClaEntry>>,
}

impl ClaRegistry {
    pub(super) fn new(
        node_ids: Arc<NodeIds>,
        rib: Arc<Rib>,
        store: Arc<Store>,
        peers: Arc<PeerTable>,
        poll_channel_depth: usize,
        tasks: hardy_async::TaskPool,
        drop_tx: flume::Sender<Weak<ClaEntry>>,
    ) -> Self {
        Self {
            node_ids,
            clas: hardy_async::sync::spin::Mutex::new(Default::default()),
            rib,
            store,
            peers,
            poll_channel_depth,
            tasks,
            drop_tx,
        }
    }

    pub async fn forward(&self, peer_id: u32, bundle: Bundle) -> core::result::Result<(), Bundle> {
        self.peers.forward(peer_id, bundle).await
    }

    pub async fn shutdown(&self) {
        let names: Vec<String> = self.clas.lock().keys().cloned().collect();

        for name in names {
            self.unregister(&name).await;
        }

        self.tasks.shutdown().await;
    }

    // Full registration in one step (for runtime dynamic registration via gRPC).
    pub async fn register(
        self: &Arc<Self>,
        name: String,
        cla: Arc<dyn Cla>,
        dispatcher: &Arc<Dispatcher>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        let address_type = cla.address_type();
        let entry = {
            let mut clas = self.clas.lock();
            let hash_map::Entry::Vacant(e) = clas.entry(name.clone()) else {
                return Err(cla::Error::AlreadyExists(name));
            };
            e.insert(Arc::new(ClaEntry {
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
        entry
            .cla
            .on_register(
                Box::new(ClaSink::new(&entry, self.clone(), dispatcher.clone())),
                &node_ids,
            )
            .await;

        metrics::gauge!("bpa.cla.registered").increment(1.0);
        info!("Registered CLA: {name}");

        Ok(node_ids)
    }

    pub(super) fn signal_dropped(&self, cla: Weak<ClaEntry>) {
        let _ = self.drop_tx.send(cla);
    }

    pub(super) async fn unregister(&self, name: &str) {
        let cla = self.clas.lock().remove(name);

        let Some(cla) = cla else {
            return;
        };

        cla.cla.on_unregister().await;

        if let Some(address_type) = cla.cla.address_type() {
            self.rib.remove_address_type(&address_type);
        }

        let peers = core::mem::take(&mut *cla.peers.lock());
        for (_, (node_ids, peer_id)) in peers {
            for node_id in node_ids {
                self.rib.remove_forward(node_id, peer_id).await;
                metrics::gauge!("bpa.fib.entries", "cla" => cla.name.clone()).decrement(1.0);
            }
            self.peers.remove(peer_id).await;
        }

        metrics::gauge!("bpa.cla.registered").decrement(1.0);
        info!("Unregistered CLA: {}", cla.name);
    }

    pub(super) async fn add_peer(
        &self,
        cla: Arc<ClaEntry>,
        dispatcher: Arc<Dispatcher>,
        cla_addr: ClaAddress,
        node_ids: &[NodeId],
    ) -> bool {
        let peer = Arc::new(Peer::new(Arc::downgrade(&cla)));

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

    pub(super) async fn remove_peer(&self, cla: Arc<ClaEntry>, cla_addr: &ClaAddress) -> bool {
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
    use crate::cla::{ForwardBundleResult, Sink};
    use hardy_async::async_trait;
    use hardy_async::sync::spin::Once;

    struct TestCla {
        sink: Once<Box<dyn Sink>>,
    }

    impl TestCla {
        fn new() -> Self {
            Self { sink: Once::new() }
        }
    }

    #[async_trait]
    impl Cla for TestCla {
        async fn on_register(&self, sink: Box<dyn Sink>, _node_ids: &[hardy_bpv7::eid::NodeId]) {
            self.sink.call_once(|| sink);
        }
        async fn on_unregister(&self) {}
        async fn forward(
            &self,
            _queue: Option<u32>,
            _cla_addr: &ClaAddress,
            _bundle: bytes::Bytes,
        ) -> cla::Result<ForwardBundleResult> {
            Ok(ForwardBundleResult::Sent)
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
        bpa.start(false);

        let cla = Arc::new(TestCla::new());
        bpa.register_cla("cascade-cla".to_string(), cla.clone(), None)
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
            .register_cla("cascade-cla".to_string(), cla2, None)
            .await;
        assert!(
            result.is_ok(),
            "Re-registration after unregister should succeed, got: {result:?}"
        );

        bpa.shutdown().await;
    }
}
