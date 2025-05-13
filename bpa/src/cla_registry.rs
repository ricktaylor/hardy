use super::*;
use std::{
    collections::{HashMap, HashSet},
    sync::Weak,
};
use tokio::sync::{Mutex, RwLock};

pub struct Cla {
    pub cla: Arc<dyn cla::Cla>,
    subnets: Mutex<HashSet<eid_pattern::EidPattern>>,
    ident: String,
}

impl PartialEq for Cla {
    fn eq(&self, other: &Self) -> bool {
        self.ident == other.ident
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
        self.ident.cmp(&other.ident)
    }
}

impl std::hash::Hash for Cla {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ident.hash(state);
    }
}

impl std::fmt::Debug for Cla {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cla")
            .field("ident", &self.ident)
            .field("subnets", &self.subnets)
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

    async fn dispatch(&self, bundle: &[u8]) -> cla::Result<()> {
        self.dispatcher.receive_bundle(bundle).await
    }

    async fn add_subnet(&self, pattern: eid_pattern::EidPattern) -> cla::Result<()> {
        let Some(cla) = self.cla.upgrade() else {
            return Err(cla::Error::Disconnected);
        };
        self.registry.add_subnet(&cla, pattern).await;
        Ok(())
    }

    async fn remove_subnet(&self, pattern: &eid_pattern::EidPattern) -> cla::Result<bool> {
        let Some(cla) = self.cla.upgrade() else {
            return Err(cla::Error::Disconnected);
        };
        Ok(self.registry.remove_subnet(&cla, pattern).await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(cla) = self.cla.upgrade() {
            tokio::runtime::Handle::current().block_on(self.registry.unregister(cla));
        }
    }
}

#[derive(Default)]
struct ClaRegistryInner {
    clas: HashMap<String, Arc<Cla>>,
    next_ids: HashMap<String, usize>,
}

pub struct ClaRegistry {
    inner: RwLock<ClaRegistryInner>,
    rib: Arc<rib::Rib>,
}

impl ClaRegistry {
    pub fn new(rib: Arc<rib::Rib>) -> Self {
        Self {
            inner: Default::default(),
            rib,
        }
    }

    pub async fn shutdown(&self) {
        for cla in self
            .inner
            .write()
            .await
            .clas
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
        {
            self.unregister_cla(cla).await;
        }
    }

    pub async fn register(
        self: &Arc<Self>,
        ident_prefix: &str,
        cla: Arc<dyn cla::Cla>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> String {
        // Scope lock
        let (cla, ident) = {
            let mut inner = self.inner.write().await;

            let next = if let Some(next) = inner.next_ids.get_mut(ident_prefix) {
                *next += 1;
                *next
            } else {
                inner.next_ids.insert(ident_prefix.into(), 0);
                0
            };

            let ident = format!("{ident_prefix}/{next}");

            let cla = Arc::new(Cla {
                cla,
                subnets: Default::default(),
                ident: ident.clone(),
            });

            inner.clas.insert(ident.clone(), cla.clone());
            (cla, ident)
        };

        info!("Registered new CLA: {ident}");

        cla.cla
            .on_register(
                ident.clone(),
                Box::new(Sink {
                    cla: Arc::downgrade(&cla),
                    registry: self.clone(),
                    dispatcher: dispatcher.clone(),
                }),
            )
            .await;

        ident
    }

    async fn unregister(&self, cla: Arc<Cla>) {
        if let Some(cla) = self.inner.write().await.clas.remove(&cla.ident) {
            self.unregister_cla(cla).await;
        }
    }

    async fn unregister_cla(&self, cla: Arc<Cla>) {
        cla.cla.on_unregister().await;

        for pattern in cla.subnets.lock().await.drain().collect::<Vec<_>>() {
            self.rib.remove_forward(&pattern, &cla.ident).await;
        }

        info!("Unregistered CLA: {}", cla.ident);
    }

    async fn add_subnet(&self, cla: &Arc<Cla>, pattern: eid_pattern::EidPattern) {
        if cla.subnets.lock().await.insert(pattern.clone()) {
            return;
        }
        self.rib.add_forward(pattern, &cla.ident, cla.clone()).await
    }

    async fn remove_subnet(&self, cla: &Cla, pattern: &eid_pattern::EidPattern) -> bool {
        if cla.subnets.lock().await.remove(pattern) {
            self.rib.remove_forward(pattern, &cla.ident).await
        } else {
            false
        }
    }
}
