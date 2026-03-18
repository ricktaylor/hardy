use bytes::Bytes;
use hardy_async::async_trait;
use hardy_bpv7::eid::NodeId;
use tracing::{debug, info};

use super::peers::{Peer, PeerTable};
use super::{Cla, ClaAddress, ClaAddressType, ClaSink, Error, Result};
use crate::bundle::Bundle;
use crate::dispatcher::Dispatcher;
use crate::hash_map::Entry;
use crate::policy::{EgressPolicy, NullEgressPolicy};
use crate::rib::Rib;
use crate::storage::Store;
use crate::{Arc, HashMap, Weak};

// CLA registry uses hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. No read-only access pattern (RwLock not needed)
// 3. No blocking/RNG/iteration while holding lock
// 4. Avoids OS mutex overhead on CLA lifecycle operations

pub struct ClaRecord {
    pub(super) cla: Arc<dyn Cla>,
    pub(super) policy: Arc<dyn EgressPolicy>,

    name: String,
    // sync::spin::Mutex for O(1) peer HashMap operations
    // Key: ClaAddress (primary key for a link-layer adjacency)
    // Value: (known EIDs for the peer, peer_id in PeerTable)
    // An empty EID vec means a Neighbour (EID not yet known; no RIB entry installed)
    peers: hardy_async::sync::spin::Mutex<HashMap<ClaAddress, (Vec<NodeId>, u32)>>,
    address_type: Option<ClaAddressType>,
}

impl PartialEq for ClaRecord {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for ClaRecord {}

impl PartialOrd for ClaRecord {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ClaRecord {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl core::hash::Hash for ClaRecord {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl core::fmt::Debug for ClaRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cla")
            .field("name", &self.name)
            .field("address_type", &self.address_type)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}

struct Sink {
    cla: Weak<ClaRecord>,
    registry: Arc<ClaRegistry>,
    dispatcher: Arc<Dispatcher>,
}

#[async_trait]
impl ClaSink for Sink {
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
    ) -> Result<()> {
        let cla_name = self.cla.upgrade().map(|c| c.name.clone().into());
        self.dispatcher
            .receive_bundle(bundle, cla_name, peer_node.cloned(), peer_addr.cloned())
            .await
    }

    async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> Result<bool> {
        let cla = self.cla.upgrade().ok_or(Error::Disconnected)?;
        Ok(self
            .registry
            .add_peer(cla, self.dispatcher.clone(), cla_addr, node_ids)
            .await)
    }

    async fn remove_peer(&self, cla_addr: &ClaAddress) -> Result<bool> {
        Ok(self
            .registry
            .remove_peer(self.cla.upgrade().ok_or(Error::Disconnected)?, cla_addr)
            .await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(cla) = self.cla.upgrade() {
            // Spawn async cleanup onto the Registry's TaskPool
            // This makes the cleanup tracked by the Registry's lifecycle
            let registry = self.registry.clone();
            hardy_async::spawn!(self.registry.tasks, "cla_drop_cleanup", async move {
                registry.unregister(cla).await;
            });
        }
    }
}

pub(crate) struct ClaRegistry {
    node_ids: Vec<NodeId>,
    // sync::spin::Mutex for O(1) CLA HashMap operations (no read-only access needed)
    records: hardy_async::sync::spin::Mutex<HashMap<String, Arc<ClaRecord>>>,
    rib: Arc<Rib>,
    store: Arc<Store>,
    peers: PeerTable,
    poll_channel_depth: usize,
    tasks: hardy_async::TaskPool,
}

impl ClaRegistry {
    pub fn new(
        node_ids: Vec<NodeId>,
        poll_channel_depth: usize,
        rib: Arc<Rib>,
        store: Arc<Store>,
    ) -> Self {
        Self {
            node_ids,
            records: Default::default(),
            rib,
            store,
            peers: PeerTable::new(),
            poll_channel_depth,
            tasks: hardy_async::TaskPool::new(),
        }
    }

    pub async fn shutdown(&self) {
        // sync::spin::Mutex::lock() returns guard directly (no Result)
        let clas = self
            .records
            .lock()
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>();

        for cla in clas {
            self.unregister_cla(cla).await;
        }

        // Wait for all cleanup tasks spawned from Drop handlers
        self.tasks.shutdown().await;
    }

    pub async fn register(
        self: &Arc<Self>,
        name: String,
        address_type: Option<ClaAddressType>,
        cla: Arc<dyn Cla>,
        dispatcher: &Arc<Dispatcher>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> Result<Vec<NodeId>> {
        // Scope lock
        let cla = {
            let mut clas = self.records.lock();
            let Entry::Vacant(e) = clas.entry(name.clone()) else {
                return Err(Error::AlreadyExists(name));
            };

            info!("Registered new CLA: {name}");

            e.insert(Arc::new(ClaRecord {
                cla,
                peers: Default::default(),
                name,
                address_type,
                policy: policy.unwrap_or_else(|| Arc::new(NullEgressPolicy::new())),
            }))
            .clone()
        };

        // Register that the CLA is a handler for the address type
        if let Some(address_type) = address_type {
            self.rib.add_address_type(address_type, cla.clone());
        }

        cla.cla
            .on_register(
                Box::new(Sink {
                    cla: Arc::downgrade(&cla),
                    registry: self.clone(),
                    dispatcher: dispatcher.clone(),
                }),
                &self.node_ids,
            )
            .await;

        Ok(self.node_ids.clone())
    }

    async fn unregister(&self, cla: Arc<ClaRecord>) {
        let cla = self.records.lock().remove(&cla.name);

        if let Some(cla) = cla {
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<ClaRecord>) {
        cla.cla.on_unregister().await;

        if let Some(address_type) = &cla.address_type {
            self.rib.remove_address_type(address_type);
        }

        let peers = core::mem::take(&mut *cla.peers.lock());

        for (_, (node_ids, peer_id)) in peers {
            // Remove RIB entries for all EIDs associated with this address
            for node_id in node_ids {
                self.rib.remove_forward(node_id, peer_id).await;
            }
            // Remove from peer table (stops forwarding, signals drain)
            self.peers.remove(peer_id).await;
        }

        // Queue pollers will exit naturally when channels are closed.
        // They're tracked by Registry's TaskPool and cleaned up in shutdown().

        info!("Unregistered CLA: {}", cla.name);
    }

    async fn add_peer(
        &self,
        cla: Arc<ClaRecord>,
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
                Entry::Vacant(e) => {
                    e.insert((node_ids.to_vec(), peer_id));
                    true
                }
                Entry::Occupied(_) => false,
            }
        };

        if !inserted {
            self.peers.remove(peer_id).await;
            return false;
        }

        debug!(
            "Added new peer {peer_id}: [{node_ids:?}] at {cla_addr} via CLA {}",
            cla.name
        );

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
        }

        true
    }

    async fn remove_peer(&self, cla: Arc<ClaRecord>, cla_addr: &ClaAddress) -> bool {
        let Some((node_ids, peer_id)) = cla.peers.lock().remove(cla_addr) else {
            return false;
        };

        self.peers.remove(peer_id).await;
        for node_id in node_ids {
            self.rib.remove_forward(node_id, peer_id).await;
        }

        debug!("Removed peer {peer_id}");

        true
    }

    pub async fn forward(&self, peer_id: u32, bundle: Bundle) {
        if let Err(bundle) = self.peers.forward(peer_id, bundle).await {
            debug!("CLA forward failed, returning bundle to watch queue");
            self.store.watch_bundle(bundle).await;
        }
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Duplicate Registration' (Register CLA with existing name)
    // #[test]
    // fn test_duplicate_registration() {
    //     todo!("Verify Register CLA with existing name");
    // }

    // // TODO: Implement test for 'Peer Lifecycle' (Verify RIB updates on peer add/remove)
    // #[test]
    // fn test_peer_lifecycle() {
    //     todo!("Verify RIB updates on peer add/remove");
    // }

    // // TODO: Implement test for 'Cascading Cleanup' (Verify unregistering CLA removes peers)
    // #[test]
    // fn test_cascading_cleanup() {
    //     todo!("Verify unregistering CLA removes peers");
    // }
}
