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
    peers: hardy_async::sync::spin::Mutex<HashMap<NodeId, HashMap<ClaAddress, u32>>>,
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
        peer_node: Option<&NodeId>,
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
    // sync::spin::Mutex for O(1) CLA HashMap operations (no read-only access needed)
    clas: hardy_async::sync::spin::Mutex<HashMap<String, Arc<Cla>>>,
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
        // sync::spin::Mutex::lock() returns guard directly (no Result)
        let clas = self.clas.lock().drain().map(|(_, v)| v).collect::<Vec<_>>();

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
            let mut clas = self.clas.lock();
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
        let cla = self.clas.lock().remove(&cla.name);

        if let Some(cla) = cla {
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<Cla>) {
        cla.cla.on_unregister().await;

        if let Some(address_type) = &cla.address_type {
            self.rib.remove_address_type(address_type);
        }

        let peers = core::mem::take(&mut *cla.peers.lock());

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

        // Acquire peer_id first (without holding cla.peers lock) to avoid nested spinlock acquisition.
        // If the cla.peers entry already exists, we clean up the orphaned peer_id.
        let peer_id = self.peers.insert(peer.clone());

        // Now try to insert into cla.peers (separate lock acquisition, no nesting)
        let inserted = {
            let mut peers = cla.peers.lock();
            match peers.entry(node_id.clone()) {
                hash_map::Entry::Occupied(mut e) => {
                    match e.get_mut().entry(cla_addr.clone()) {
                        hash_map::Entry::Vacant(inner_e) => {
                            inner_e.insert(peer_id);
                            true
                        }
                        hash_map::Entry::Occupied(_) => false, // Already exists
                    }
                }
                hash_map::Entry::Vacant(e) => {
                    e.insert([(cla_addr.clone(), peer_id)].into());
                    true
                }
            }
        };

        // If entry already existed, clean up the orphaned peer_id
        if !inserted {
            self.peers.remove(peer_id).await;
            return false;
        }

        debug!(
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
            .get_mut(&node_id)
            .and_then(|m| m.remove(cla_addr))
        else {
            return false;
        };

        self.peers.remove(peer_id).await;
        self.rib.remove_forward(node_id, peer_id).await;

        debug!("Removed peer {peer_id}");

        true
    }

    pub async fn forward(&self, peer_id: u32, bundle: bundle::Bundle) {
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
