use super::*;
use rand::Rng;
use std::collections::HashMap;
use tokio::sync::RwLock;

pub struct Cla {
    ident: String,
    protocol: String,
    cla: Arc<dyn cla::Cla>,
    connected: connected::ConnectedFlag,
}

impl std::cmp::PartialEq for Cla {
    fn eq(&self, other: &Self) -> bool {
        matches!(self.cmp(other), std::cmp::Ordering::Equal)
    }
}

impl std::cmp::Eq for Cla {}

impl std::cmp::PartialOrd for Cla {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for Cla {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap()
    }
}

impl std::fmt::Debug for Cla {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cla")
            .field("ident", &self.ident)
            .field("protocol", &self.protocol)
            .field("connected", &self.connected)
            .finish()
    }
}

impl std::fmt::Display for Cla {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.protocol, self.ident)
    }
}

impl Cla {
    pub async fn forward(
        &self,
        destination: &bpv7::Eid,
        addr: Option<&[u8]>,
        data: &[u8],
    ) -> cla::Result<cla::ForwardBundleResult> {
        if self.connected.is_connected() {
            self.cla.forward(destination, addr, data).await
        } else {
            Err(cla::Error::Disconnected)
        }
    }
}

struct Sink {
    registry: Arc<ClaRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    handle: u32,
}

#[async_trait]
impl cla::Sink for Sink {
    async fn disconnect(&self) {
        self.registry.unregister(self.handle).await
    }

    async fn dispatch(&self, data: &[u8]) -> cla::Result<()> {
        self.dispatcher
            .receive_bundle(data)
            .await
            .map_err(Into::into)
    }

    async fn confirm_forwarding(&self, bundle_id: &bpv7::BundleId) -> cla::Result<()> {
        self.dispatcher
            .confirm_forwarding(self.handle, bundle_id)
            .await
    }

    async fn add_neighbour(
        &self,
        destination: &bpv7::Eid,
        addr: Option<&[u8]>,
        priority: u32,
    ) -> cla::Result<()> {
        self.registry
            .add_neighbour(self.handle, destination, addr, priority)
            .await
    }

    async fn remove_neighbour(&self, destination: &bpv7::Eid) {
        self.registry
            .remove_neighbour(self.handle, destination)
            .await
    }
}

pub struct ClaRegistry {
    clas: RwLock<HashMap<u32, Arc<Cla>>>,
    fib: Arc<fib_impl::Fib>,
}

impl ClaRegistry {
    pub fn new(fib: Arc<fib_impl::Fib>) -> Self {
        Self {
            clas: RwLock::new(HashMap::new()),
            fib,
        }
    }

    pub async fn shutdown(&self) {
        for (_, cla) in self.clas.write().await.drain() {
            cla.connected.disconnect();
            cla.cla.on_disconnect().await;

            info!("Unregistered CLA: {}/{}", cla.protocol, cla.ident);
        }
    }

    pub async fn register(
        self: &Arc<Self>,
        ident: &str,
        kind: &str,
        cla: Arc<dyn cla::Cla>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> cla::Result<()> {
        // Scope lock
        let (cla, handle) = {
            let mut clas = self.clas.write().await;

            // Compose a handle
            let mut rng = rand::thread_rng();
            let mut handle = rng.gen::<std::num::NonZeroU32>().into();

            // Check handle is unique
            while clas.contains_key(&handle) {
                handle = rng.gen::<std::num::NonZeroU32>().into();
            }

            // Confirm the ident is unique
            if clas.values().any(|cla| cla.ident == ident) {
                return Err(cla::Error::DuplicateClaIdent(ident.to_string()));
            }

            let cla = Arc::new(Cla {
                ident: ident.to_string(),
                protocol: kind.to_string(),
                cla,
                connected: connected::ConnectedFlag::default(),
            });

            clas.insert(handle, cla.clone());
            (cla, handle)
        };

        if let Err(e) = cla
            .cla
            .on_connect(Box::new(Sink {
                registry: self.clone(),
                dispatcher: dispatcher.clone(),
                handle,
            }))
            .await
        {
            // Connect failed
            self.clas.write().await.remove(&handle);
            return Err(e);
        }

        info!("Registered new CLA: {}/{}", kind, ident);

        cla.connected.connect();

        Ok(())
    }

    #[instrument(skip(self))]
    async fn unregister(&self, handle: u32) {
        let cla = self.clas.write().await.remove(&handle);
        if let Some(cla) = cla {
            cla.connected.disconnect();
            cla.cla.on_disconnect().await;

            info!("Unregistered CLA: {}/{}", cla.protocol, cla.ident);
        }
    }

    async fn add_neighbour(
        &self,
        handle: u32,
        destination: &bpv7::Eid,
        addr: Option<&[u8]>,
        priority: u32,
    ) -> cla::Result<()> {
        let cla = self
            .clas
            .read()
            .await
            .get(&handle)
            .cloned()
            .ok_or(cla::Error::Disconnected)?;

        self.fib
            .add_neighbour(destination, addr, priority, cla)
            .await
            .map_err(Into::into)
    }

    async fn remove_neighbour(&self, handle: u32, destination: &bpv7::Eid) {
        if let Some(cla) = self.clas.read().await.get(&handle).cloned() {
            self.fib.remove_neighbour(&cla, destination).await
        }
    }
}
