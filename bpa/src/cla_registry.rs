use super::*;
use std::collections::{HashMap, HashSet};
use tokio::sync::{Mutex, RwLock};

pub struct Cla {
    cla: Arc<dyn cla::Cla>,
    connected: connected::ConnectedFlag,
    subnets: Mutex<HashSet<eid_pattern::EidPattern>>,
}

impl Cla {
    pub async fn forward(
        &self,
        destination: &bpv7::Eid,
        data: &[u8],
    ) -> cla::Result<cla::ForwardBundleResult> {
        if !self.connected.is_connected() {
            return Err(cla::Error::Disconnected);
        }
        self.cla.forward(destination, data).await
    }
}

struct Sink {
    registry: Arc<ClaRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    ident: String,
}

#[async_trait]
impl cla::Sink for Sink {
    async fn disconnect(&self) {
        self.registry.unregister(&self.ident).await
    }

    async fn dispatch(&self, data: &[u8]) -> cla::Result<()> {
        self.dispatcher.receive_bundle(data).await
    }

    async fn add_subnet(&self, pattern: eid_pattern::EidPattern) {
        self.registry.add_subnet(&self.ident, pattern).await
    }

    async fn remove_subnet(&self, pattern: &eid_pattern::EidPattern) -> bool {
        self.registry.remove_subnet(&self.ident, pattern).await
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
        let e = { self.inner.write().await.clas.drain().collect::<Vec<_>>() };
        for (ident, cla) in e {
            cla.connected.disconnect();
            cla.cla.on_disconnect().await;

            info!("Unregistered CLA: {ident}");
        }
    }

    pub async fn register(
        self: &Arc<Self>,
        ident_prefix: &str,
        cla: Arc<dyn cla::Cla>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> cla::Result<String> {
        // Scope lock
        let (cla, ident) = {
            let mut inner = self.inner.write().await;

            // Incrememnt
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
                connected: connected::ConnectedFlag::default(),
                subnets: Default::default(),
            });

            inner.clas.insert(ident.clone(), cla.clone());
            (cla, ident)
        };

        info!("Registered new CLA: {ident}");

        if let Err(e) = cla
            .cla
            .on_connect(
                &ident,
                Box::new(Sink {
                    registry: self.clone(),
                    dispatcher: dispatcher.clone(),
                    ident: ident.clone(),
                }),
            )
            .await
        {
            // Connect failed
            info!("New CLA {ident} failed to connect {}", e.to_string());

            if let Some(cla) = self.inner.write().await.clas.remove(&ident) {
                for pattern in cla.subnets.lock().await.drain() {
                    self.rib.remove_forward(&pattern, &ident).await;
                }

                info!("Unregistered CLA: {ident}");
            }
            return Err(e);
        }

        cla.connected.connect();

        Ok(ident)
    }

    #[instrument(skip(self))]
    async fn unregister(&self, ident: &str) {
        let cla = {
            self.inner
                .write()
                .await
                .clas
                .remove(ident)
                .trace_expect("Invalid CLA ident: {ident}")
        };

        for pattern in cla.subnets.lock().await.drain() {
            self.rib.remove_forward(&pattern, ident).await;
        }

        cla.connected.disconnect();
        cla.cla.on_disconnect().await;

        info!("Unregistered CLA: {ident}");
    }

    #[instrument(skip(self))]
    async fn add_subnet(&self, ident: &str, pattern: eid_pattern::EidPattern) {
        let cla = {
            self.inner
                .read()
                .await
                .clas
                .get(ident)
                .trace_expect("Invalid CLA ident: {ident}")
                .clone()
        };

        {
            if cla.subnets.lock().await.insert(pattern.clone()) {
                return;
            }
        }

        self.rib.add_forward(pattern, ident).await
    }

    async fn remove_subnet(&self, ident: &str, pattern: &eid_pattern::EidPattern) -> bool {
        let cla = {
            self.inner
                .read()
                .await
                .clas
                .get(ident)
                .trace_expect("Invalid CLA ident: {ident}")
                .clone()
        };

        let exists = { cla.subnets.lock().await.remove(pattern) };
        if exists {
            self.rib.remove_forward(pattern, ident).await
        } else {
            false
        }
    }

    pub async fn forward(
        &self,
        ident: &str,
        destination: &bpv7::Eid,
        data: &[u8],
    ) -> cla::Result<cla::ForwardBundleResult> {
        let Some(cla) = self.inner.read().await.clas.get(ident).cloned() else {
            return Err(cla::Error::Disconnected);
        };

        cla.forward(destination, data).await
    }
}
