use super::*;
use hardy_bpv7::eid::Eid;
use std::{
    collections::HashMap,
    ops::DerefMut,
    sync::{Mutex, RwLock, Weak},
};

pub struct Cla {
    cla: Arc<dyn cla::Cla>,
    name: String,
    peers: Mutex<HashMap<Eid, HashMap<ClaAddress, u32>>>,
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

    async fn add_peer(&self, eid: Eid, addr: ClaAddress) -> cla::Result<()> {
        self.registry
            .add_peer(
                self.cla.upgrade().ok_or(cla::Error::Disconnected)?,
                eid,
                addr,
            )
            .await;
        Ok(())
    }

    async fn remove_peer(&self, eid: &Eid, addr: &ClaAddress) -> cla::Result<bool> {
        Ok(self.registry.remove_peer(
            self.cla.upgrade().ok_or(cla::Error::Disconnected)?,
            eid,
            addr,
        ))
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
    node_ids: Vec<Eid>,
    clas: RwLock<HashMap<String, Arc<Cla>>>,
    rib: Arc<rib::Rib>,
    peers: peers::PeerTable,
}

impl Registry {
    pub fn new(config: &config::Config, rib: Arc<rib::Rib>) -> Self {
        Self {
            node_ids: (&config.node_ids).into(),
            clas: Default::default(),
            rib,
            peers: peers::PeerTable::new(),
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
                    });

                    e.insert(cla.clone());
                    cla
                }
            }
        };

        if let Err(e) = cla
            .cla
            .on_register(
                Box::new(Sink {
                    cla: Arc::downgrade(&cla),
                    registry: self.clone(),
                    dispatcher: dispatcher.clone(),
                }),
                &self.node_ids,
            )
            .await
        {
            // Remove the CLA
            self.clas
                .write()
                .trace_expect("Failed to lock mutex")
                .remove(&name);
            return Err(e);
        }

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

        let peers = std::mem::take(
            cla.peers
                .lock()
                .trace_expect("Failed to lock mutex")
                .deref_mut(),
        );

        for (eid, i) in peers {
            for peer_id in i.values() {
                self.rib.remove_forward(&eid, *peer_id);
                self.peers.remove(*peer_id);
            }
        }

        info!("Unregistered CLA: {}", cla.name);
    }

    async fn add_peer(&self, cla: Arc<Cla>, eid: Eid, addr: ClaAddress) -> bool {
        // We search here because it results in better lookups than linear searching the peers table
        let peer_id = {
            match cla
                .peers
                .lock()
                .trace_expect("Failed to lock mutex")
                .entry(eid.clone())
            {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    match e.get_mut().entry(addr.clone()) {
                        std::collections::hash_map::Entry::Occupied(_) => return false,
                        std::collections::hash_map::Entry::Vacant(e) => {
                            let peer_id = self.peers.insert(cla.clone(), eid.clone(), addr);
                            e.insert(peer_id);
                            peer_id
                        }
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    let peer_id = self.peers.insert(cla.clone(), eid.clone(), addr.clone());
                    e.insert([(addr, peer_id)].into());
                    peer_id
                }
            }
        };

        self.rib.add_forward(eid, peer_id).await;

        true
    }

    fn remove_peer(&self, cla: Arc<Cla>, eid: &Eid, addr: &ClaAddress) -> bool {
        let Some(peer_id) = cla
            .peers
            .lock()
            .trace_expect("Failed to lock mutex")
            .get_mut(eid)
            .and_then(|m| m.remove(addr))
        else {
            return false;
        };

        self.rib.remove_forward(eid, peer_id);

        self.peers.remove(peer_id);

        true
    }
}
