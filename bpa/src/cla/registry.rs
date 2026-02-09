use super::*;
use hardy_bpv7::eid::NodeId;
use std::sync::{Mutex, RwLock};

pub struct Cla {
    pub(super) cla: Arc<dyn cla::Cla>,
    pub(super) policy: Arc<dyn policy::EgressPolicy>,

    name: String,
    peers: Mutex<HashMap<NodeId, HashMap<ClaAddress, u32>>>,
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
            .finish()
    }
}

struct Sink {
    cla: Weak<Cla>,
    registry: Arc<Registry>,
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
        peer_node: Option<&hardy_bpv7::eid::NodeId>,
        peer_addr: Option<&ClaAddress>,
    ) -> cla::Result<()> {
        let cla_name = self.cla.upgrade().map(|c| c.name.clone().into());
        self.dispatcher
            .receive_bundle(bundle, cla_name, peer_node.cloned(), peer_addr.cloned())
            .await
    }

    async fn add_peer(&self, node_id: NodeId, cla_addr: ClaAddress) -> cla::Result<bool> {
        let cla = self.cla.upgrade().ok_or(cla::Error::Disconnected)?;
        Ok(self
            .registry
            .add_peer(cla, self.dispatcher.clone(), node_id, cla_addr)
            .await)
    }

    async fn remove_peer(&self, node_id: NodeId, cla_addr: &ClaAddress) -> cla::Result<bool> {
        Ok(self
            .registry
            .remove_peer(
                self.cla.upgrade().ok_or(cla::Error::Disconnected)?,
                node_id,
                cla_addr,
            )
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

pub(crate) struct Registry {
    node_ids: Vec<NodeId>,
    clas: RwLock<HashMap<String, Arc<Cla>>>,
    rib: Arc<rib::Rib>,
    store: Arc<storage::Store>,
    peers: peers::PeerTable,
    poll_channel_depth: usize,
    tasks: hardy_async::TaskPool,
}

impl Registry {
    pub fn new(config: &config::Config, rib: Arc<rib::Rib>, store: Arc<storage::Store>) -> Self {
        Self {
            node_ids: (&config.node_ids).into(),
            clas: Default::default(),
            rib,
            store,
            peers: peers::PeerTable::new(),
            poll_channel_depth: config.poll_channel_depth.into(),
            tasks: hardy_async::TaskPool::new(),
        }
    }

    pub async fn shutdown(&self) {
        let clas = self
            .clas
            .write()
            .trace_expect("Failed to lock mutex")
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
        cla: Arc<dyn cla::Cla>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
        policy: Option<Arc<dyn policy::EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        // Scope lock
        let cla = {
            let mut clas = self.clas.write().trace_expect("Failed to lock mutex");
            let hash_map::Entry::Vacant(e) = clas.entry(name.clone()) else {
                return Err(cla::Error::AlreadyExists(name));
            };

            info!("Registered new CLA: {name}");

            e.insert(Arc::new(Cla {
                cla,
                peers: Default::default(),
                name,
                address_type,
                policy: policy
                    .unwrap_or_else(|| Arc::new(policy::null_policy::EgressPolicy::new())),
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

    async fn unregister(&self, cla: Arc<Cla>) {
        let cla = self
            .clas
            .write()
            .trace_expect("Failed to lock mutex")
            .remove(&cla.name);

        if let Some(cla) = cla {
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<Cla>) {
        cla.cla.on_unregister().await;

        if let Some(address_type) = &cla.address_type {
            self.rib.remove_address_type(address_type);
        }

        let peers = core::mem::take(&mut *cla.peers.lock().trace_expect("Failed to lock mutex"));

        for (node_id, peers) in peers {
            for (_, peer_id) in peers {
                // Remove from RIB first (stops new routing), then close channel (signals drain)
                self.rib.remove_forward(node_id.clone(), peer_id).await;
                self.peers.remove(peer_id).await;
            }
        }

        // Queue pollers will exit naturally when channels are closed.
        // They're tracked by Registry's TaskPool and cleaned up in shutdown().

        info!("Unregistered CLA: {}", cla.name);
    }

    async fn add_peer(
        &self,
        cla: Arc<Cla>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        node_id: NodeId,
        cla_addr: ClaAddress,
    ) -> bool {
        // TODO: This should ideally do a replace and return the previous

        let peer = Arc::new(peers::Peer::new(Arc::downgrade(&cla)));

        // We search here because it results in better lookups than linear searching the peers table
        let peer_id = {
            match cla
                .peers
                .lock()
                .trace_expect("Failed to lock mutex")
                .entry(node_id.clone())
            {
                hash_map::Entry::Occupied(mut e) => {
                    let hash_map::Entry::Vacant(e) = e.get_mut().entry(cla_addr.clone()) else {
                        return false;
                    };
                    *e.insert(self.peers.insert(peer.clone()))
                }
                hash_map::Entry::Vacant(e) => {
                    let peer_id = self.peers.insert(peer.clone());
                    e.insert([(cla_addr.clone(), peer_id)].into());
                    peer_id
                }
            }
        };

        info!(
            "Added new peer {peer_id}: {node_id} at {cla_addr} via CLA {}",
            cla.name
        );

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

        // Add to the RIB
        self.rib.add_forward(node_id, peer_id).await;

        true
    }

    async fn remove_peer(&self, cla: Arc<Cla>, node_id: NodeId, cla_addr: &ClaAddress) -> bool {
        let Some(peer_id) = cla
            .peers
            .lock()
            .trace_expect("Failed to lock mutex")
            .get_mut(&node_id)
            .and_then(|m| m.remove(cla_addr))
        else {
            return false;
        };

        self.peers.remove(peer_id).await;
        self.rib.remove_forward(node_id, peer_id).await;

        info!("Removed peer {peer_id}");

        true
    }

    pub async fn forward(&self, peer_id: u32, bundle: bundle::Bundle) {
        if let Err(bundle) = self.peers.forward(peer_id, bundle).await {
            info!("CLA Failed to forward bundle");
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
