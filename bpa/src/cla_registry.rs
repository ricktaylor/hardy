use super::*;
use hardy_bpv7::eid::Eid;
use std::{
    collections::HashMap,
    sync::{Mutex, RwLock, Weak},
};

pub struct Cla {
    pub cla: Arc<dyn cla::Cla>,
    pub name: String,
    peers: Mutex<HashMap<Eid, cla::ClaAddress>>,
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

    async fn add_peer(&self, eid: Eid, addr: cla::ClaAddress) -> cla::Result<()> {
        let Some(cla) = self.cla.upgrade() else {
            return Err(cla::Error::Disconnected);
        };
        self.registry.add_peer(&cla, eid, addr).await;
        Ok(())
    }

    async fn remove_peer(&self, eid: &Eid) -> cla::Result<bool> {
        let Some(cla) = self.cla.upgrade() else {
            return Err(cla::Error::Disconnected);
        };
        Ok(self.registry.remove_peer(&cla, eid))
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
    node_ids: Vec<Eid>,
    clas: RwLock<HashMap<String, Arc<Cla>>>,
    rib: Arc<rib::Rib>,
}

impl ClaRegistry {
    pub fn new(config: &config::Config, rib: Arc<rib::Rib>) -> Self {
        Self {
            node_ids: (&config.node_ids).into(),
            clas: Default::default(),
            rib,
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
        address_type: Option<cla::ClaAddressType>,
        cla: Arc<dyn cla::Cla>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> cla::Result<()> {
        // Scope lock
        let cla = {
            let mut clas = self.clas.write().trace_expect("Failed to lock mutex");

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

            clas.insert(name.clone(), cla.clone());
            cla
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

        let clas = cla
            .peers
            .lock()
            .trace_expect("Failed to lock mutex")
            .drain()
            .collect::<Vec<_>>();

        for (eid, cla_addr) in clas {
            self.rib.remove_forward(&eid, &cla_addr, Some(&cla));
        }

        info!("Unregistered CLA: {}", cla.name);
    }

    async fn add_peer(&self, cla: &Arc<Cla>, eid: Eid, addr: cla::ClaAddress) {
        if cla
            .peers
            .lock()
            .trace_expect("Failed to lock mutex")
            .insert(eid.clone(), addr.clone())
            .is_none()
        {
            self.rib.add_forward(eid, addr, Some(cla.clone()))
        }
    }

    fn remove_peer(&self, cla: &Arc<Cla>, eid: &Eid) -> bool {
        let cla_addr = cla
            .peers
            .lock()
            .trace_expect("Failed to lock mutex")
            .remove(eid);

        if let Some(cla_addr) = cla_addr {
            self.rib.remove_forward(eid, &cla_addr, Some(cla))
        } else {
            false
        }
    }
}
