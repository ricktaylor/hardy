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

impl Cla {
    /// Forward `data` bytes directly to a specific CLA address without going through the
    /// egress queue or the RIB. Used by the BP-ARP subsystem to send probes to Neighbours
    /// that have no route installed yet.
    pub(crate) async fn forward_raw(
        &self,
        cla_addr: &ClaAddress,
        data: Bytes,
    ) -> cla::Result<cla::ForwardBundleResult> {
        self.cla.forward(None, cla_addr, data).await
    }
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
    node_ids: Arc<node_ids::NodeIds>,
    // sync::spin::Mutex for O(1) CLA HashMap operations (no read-only access needed)
    clas: hardy_async::sync::spin::Mutex<HashMap<String, Arc<Cla>>>,
    rib: Arc<rib::Rib>,
    store: Arc<storage::Store>,
    peers: peers::PeerTable,
    poll_channel_depth: usize,
    tasks: hardy_async::TaskPool,
    arp: Option<Arc<arp::ArpSubsystem>>,
}

impl Registry {
    pub fn new(
        node_ids: Arc<node_ids::NodeIds>,
        poll_channel_depth: usize,
        rib: Arc<rib::Rib>,
        store: Arc<storage::Store>,
        arp: Option<Arc<arp::ArpSubsystem>>,
    ) -> Self {
        Self {
            node_ids,
            clas: Default::default(),
            rib,
            store,
            peers: peers::PeerTable::new(),
            poll_channel_depth,
            tasks: hardy_async::TaskPool::new(),
            arp,
        }
    }

    /// Returns all admin endpoint EIDs for this node, used to populate BP-ARP ack payloads.
    pub fn all_admin_endpoints(&self) -> Vec<hardy_bpv7::eid::Eid> {
        self.node_ids.get_all_admin_endpoints()
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
                &Vec::<NodeId>::from(&*self.node_ids),
            )
            .await;

        Ok(Vec::<NodeId>::from(&*self.node_ids))
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

        for (_, (node_ids, peer_id)) in peers {
            // Remove RIB entries for all EIDs associated with this address
            for node_id in node_ids {
                self.rib.remove_forward(node_id, peer_id).await;
            }
            // Notify ARP subsystem so it can cancel any outstanding probe task
            if let Some(arp) = &self.arp {
                arp.on_neighbour_removed(peer_id).await;
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
        if !inserted {
            self.peers.remove(peer_id).await;
            return false;
        }

        debug!(
            "Added new peer {peer_id}: [{node_ids:?}] at {cla_addr} via CLA {}",
            cla.name
        );

        // Start the peer polling the queue
        peer.start(
            self.poll_channel_depth,
            cla.clone(),
            peer_id,
            cla_addr.clone(),
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

        // Notify BP-ARP subsystem about new Neighbour (no EID known yet)
        if node_ids.is_empty() {
            if let Some(arp) = &self.arp {
                arp.on_neighbour_added(peer_id, cla, cla_addr, &self.tasks)
                    .await;
            }
        }

        true
    }

    async fn remove_peer(&self, cla: Arc<Cla>, cla_addr: &ClaAddress) -> bool {
        let Some((node_ids, peer_id)) = cla.peers.lock().remove(cla_addr) else {
            return false;
        };

        // Notify ARP if this was a Neighbour (no EIDs known)
        if node_ids.is_empty() {
            if let Some(arp) = &self.arp {
                arp.on_neighbour_removed(peer_id).await;
            }
        }

        self.peers.remove(peer_id).await;
        for node_id in node_ids {
            self.rib.remove_forward(node_id, peer_id).await;
        }

        debug!("Removed peer {peer_id}");

        true
    }

    /// Promotes a Neighbour (peer with unknown EID) to a named Peer by learning its EID
    /// from an incoming BP-ARP message. Installs a RIB route and cancels the probe task.
    ///
    /// This is idempotent: if the Neighbour is already a Peer (EID already known),
    /// or if the address is not found, this is a no-op.
    pub async fn promote_neighbour(&self, cla_addr: &ClaAddress, eids: Vec<hardy_bpv7::eid::Eid>) {
        // Filter to valid node admin endpoint IDs only.
        let node_ids: Vec<NodeId> = eids
            .into_iter()
            .filter_map(|eid| NodeId::try_from(eid).ok())
            .collect();

        if node_ids.is_empty() {
            debug!("BP-ARP: cannot promote Neighbour — no valid node admin endpoints in EID list");
            return;
        }

        // Find the CLA owning this address and update the peer entry.
        let clas: Vec<Arc<Cla>> = self.clas.lock().values().cloned().collect();
        for cla in clas {
            let maybe_promote = {
                let mut peers = cla.peers.lock();
                if let Some(entry) = peers.get_mut(cla_addr) {
                    let was_neighbour = entry.0.is_empty();
                    // Collect EIDs that are new for this peer.
                    let new_ids: Vec<NodeId> = node_ids
                        .iter()
                        .filter(|id| !entry.0.contains(id))
                        .cloned()
                        .collect();
                    for id in &new_ids {
                        entry.0.push(id.clone());
                    }
                    if !new_ids.is_empty() {
                        Some((entry.1, new_ids, was_neighbour))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            if let Some((peer_id, new_ids, was_neighbour)) = maybe_promote {
                for node_id in &new_ids {
                    self.rib.add_forward(node_id.clone(), peer_id).await;
                    info!("BP-ARP: promoted peer {peer_id} at {cla_addr} to {node_id}");
                }
                if was_neighbour {
                    // Cancel any outstanding ARP probe now that we have at least one EID.
                    if let Some(arp) = &self.arp {
                        arp.on_ack_received(peer_id).await;
                    }
                }
                return;
            }
        }

        debug!("BP-ARP: promote_neighbour called for unknown address {cla_addr}");
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
