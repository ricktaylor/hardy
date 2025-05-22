use super::*;
use std::{collections::HashMap, sync::Weak};
use tokio::sync::{Mutex, RwLock};

pub struct Cla {
    pub cla: Arc<dyn cla::Cla>,
    name: String,
    peers: Mutex<HashMap<bpv7::Eid, cla::ClaAddress>>,
    address_type: Option<cla::ClaAddressType>,
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
    registry: Arc<ClaRegistry>,
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

    async fn add_peer(&self, eid: bpv7::Eid, addr: cla::ClaAddress) -> cla::Result<()> {
        let Some(cla) = self.cla.upgrade() else {
            return Err(cla::Error::Disconnected);
        };
        self.registry.add_peer(&cla, eid, addr).await;
        Ok(())
    }

    async fn remove_peer(&self, eid: &bpv7::Eid) -> cla::Result<bool> {
        let Some(cla) = self.cla.upgrade() else {
            return Err(cla::Error::Disconnected);
        };
        Ok(self.registry.remove_peer(&cla, eid).await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(cla) = self.cla.upgrade() {
            tokio::runtime::Handle::current().block_on(self.registry.unregister(cla));
        }
    }
}

pub struct ClaRegistry {
    clas: RwLock<HashMap<String, Arc<Cla>>>,
    rib: Arc<rib::Rib>,
}

impl ClaRegistry {
    pub fn new(rib: Arc<rib::Rib>) -> Self {
        Self {
            clas: Default::default(),
            rib,
        }
    }

    pub async fn shutdown(&self) {
        for cla in self
            .clas
            .write()
            .await
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
        {
            self.unregister_cla(cla).await;
        }
    }

    pub async fn register(
        self: &Arc<Self>,
        name: String,
        address_type: Option<cla::ClaAddressType>,
        cla: Arc<dyn cla::Cla>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> cla::Result<()> {
        // Scope lock
        let cla = {
            let mut clas = self.clas.write().await;

            if clas.contains_key(&name) {
                return Err(cla::Error::AlreadyExists(name));
            }

            let cla = Arc::new(Cla {
                cla,
                peers: Default::default(),
                name: name.clone(),
                address_type,
            });

            info!("Registered new CLA: {name}");

            clas.insert(name, cla.clone());
            cla
        };

        cla.cla
            .on_register(Box::new(Sink {
                cla: Arc::downgrade(&cla),
                registry: self.clone(),
                dispatcher: dispatcher.clone(),
            }))
            .await;

        // Register that the CLA is a handler for the address type
        if let Some(address_type) = address_type {
            self.rib.add_address_type(address_type, cla.clone()).await;
        }

        Ok(())
    }

    async fn unregister(&self, cla: Arc<Cla>) {
        if let Some(cla) = self.clas.write().await.remove(&cla.name) {
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<Cla>) {
        cla.cla.on_unregister().await;

        if let Some(address_type) = &cla.address_type {
            self.rib.remove_address_type(address_type).await;
        }

        for (eid, cla_addr) in cla.peers.lock().await.drain().collect::<Vec<_>>() {
            self.rib.remove_forward(&eid, &cla_addr, Some(&cla)).await;
        }

        info!("Unregistered CLA: {}", cla.name);
    }

    async fn add_peer(&self, cla: &Arc<Cla>, eid: bpv7::Eid, addr: cla::ClaAddress) {
        if cla
            .peers
            .lock()
            .await
            .insert(eid.clone(), addr.clone())
            .is_some()
        {
            return;
        }
        self.rib.add_forward(eid, addr, Some(cla.clone())).await
    }

    async fn remove_peer(&self, cla: &Arc<Cla>, eid: &bpv7::Eid) -> bool {
        if let Some(cla_addr) = cla.peers.lock().await.remove(eid) {
            self.rib.remove_forward(eid, &cla_addr, Some(cla)).await
        } else {
            false
        }
    }
}
