use super::*;
use hardy_bpv7::eid::NodeId;
use std::sync::{Mutex, RwLock, Weak};

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
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cla {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl std::hash::Hash for Cla {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl std::fmt::Debug for Cla {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
            self.registry.unregister(cla).await
        }
    }

    async fn dispatch(&self, bundle: Bytes) -> cla::Result<()> {
        self.dispatcher.receive_bundle(bundle).await
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
            tokio::runtime::Handle::current().block_on(self.registry.unregister(cla));
        }
    }
}

pub struct Registry {
    node_ids: Vec<NodeId>,
    clas: RwLock<HashMap<String, Arc<Cla>>>,
    rib: Arc<rib::Rib>,
    store: Arc<storage::Store>,
    peers: peers::PeerTable,
    poll_channel_depth: usize,
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
    }

    pub async fn register(
        self: &Arc<Self>,
        name: String,
        address_type: Option<ClaAddressType>,
        cla: Arc<dyn cla::Cla>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
        policy: Option<Arc<dyn policy::EgressPolicy>>,
    ) -> cla::Result<()> {
        // Scope lock
        let cla = {
            let mut clas = self.clas.write().trace_expect("Failed to lock mutex");
            match clas.entry(name.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    return Err(cla::Error::AlreadyExists(name));
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    info!("Registered new CLA: {name}");

                    let cla = Arc::new(Cla {
                        cla,
                        peers: Default::default(),
                        name: name.clone(),
                        address_type,
                        policy: policy
                            .unwrap_or_else(|| Arc::new(policy::null_policy::EgressPolicy::new())),
                    });

                    e.insert(cla.clone());
                    cla
                }
            }
        };

        cla.cla
            .on_register(
                Box::new(Sink {
                    cla: Arc::downgrade(&cla),
                    registry: self.clone(),
                    dispatcher: dispatcher.clone(),
                }),
                &self.node_ids,
            )
            .await
            .inspect_err(|_| {
                // Remove the CLA
                self.clas
                    .write()
                    .trace_expect("Failed to lock mutex")
                    .remove(&name);
            })?;

        // Register that the CLA is a handler for the address type
        if let Some(address_type) = address_type {
            self.rib.add_address_type(address_type, cla.clone());
        }

        Ok(())
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

        let peers = std::mem::take(&mut *cla.peers.lock().trace_expect("Failed to lock mutex"));

        for (node_id, i) in peers {
            for (_, peer_id) in i {
                self.peers.remove(peer_id).await;
                self.rib.remove_forward(node_id.clone(), peer_id).await;
            }
        }

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

        let peer = Arc::new(peers::Peer::new(std::sync::Arc::downgrade(&cla)));

        // We search here because it results in better lookups than linear searching the peers table
        let peer_id = {
            match cla
                .peers
                .lock()
                .trace_expect("Failed to lock mutex")
                .entry(node_id.clone())
            {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    match e.get_mut().entry(cla_addr.clone()) {
                        std::collections::hash_map::Entry::Occupied(_) => {
                            return false;
                        }
                        std::collections::hash_map::Entry::Vacant(e) => {
                            let peer_id = self.peers.insert(peer.clone());
                            e.insert(peer_id);
                            peer_id
                        }
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
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
