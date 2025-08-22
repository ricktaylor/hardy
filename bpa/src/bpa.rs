use super::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Bpa {
    store: Arc<store::Store>,
    sentinel: Arc<sentinel::Sentinel>,
    rib: Arc<rib::Rib>,
    cla_registry: Arc<cla_registry::ClaRegistry>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Bpa {
    #[instrument]
    pub async fn start(config: &config::Config) -> Result<Self, Error> {
        info!("Starting new BPA");

        if config.status_reports {
            warn!("Bundle status reports are enabled");
        }

        // New store
        let store = Arc::new(store::Store::new(config));

        // New sentinel
        let (sentinel, sentinel_rx) = sentinel::Sentinel::new(store.clone());
        let sentinel = Arc::new(sentinel);

        // New RIB
        let rib = Arc::new(rib::Rib::new(config, sentinel.clone()));

        // New registries
        let cla_registry = Arc::new(cla_registry::ClaRegistry::new(config, rib.clone()));
        let service_registry =
            Arc::new(service_registry::ServiceRegistry::new(config, rib.clone()));

        // New dispatcher
        let dispatcher = Arc::new(dispatcher::Dispatcher::new(
            config,
            store.clone(),
            sentinel.clone(),
            service_registry.clone(),
            rib.clone(),
        ));

        // Start the sentinel
        sentinel.start(dispatcher.clone(), sentinel_rx);

        // And finally restart the store
        store.start(dispatcher.clone());

        info!("BPA started");

        Ok(Self {
            store,
            sentinel,
            rib,
            cla_registry,
            service_registry,
            dispatcher,
        })
    }

    // TODO: Make this a Drop impl
    #[instrument(skip(self))]
    pub async fn shutdown(&self) {
        trace!("Shutting down BPA");

        self.dispatcher.shutdown().await;
        self.service_registry.shutdown().await;
        self.cla_registry.shutdown().await;
        self.sentinel.shutdown().await;
        self.store.shutdown().await;

        trace!("BPA stopped");
    }

    #[instrument(level = "trace", skip(self, service))]
    pub async fn register_service(
        &self,
        service_id: Option<service::ServiceId<'_>>,
        service: Arc<dyn service::Service>,
    ) -> service::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register(service_id, service, &self.dispatcher)
            .await
    }

    #[instrument(level = "trace", skip(self, cla))]
    pub async fn register_cla(
        &self,
        name: String,
        address_type: Option<cla::ClaAddressType>,
        cla: Arc<dyn cla::Cla>,
    ) -> cla::Result<()> {
        self.cla_registry
            .register(name, address_type, cla, &self.dispatcher)
            .await
    }

    #[instrument(level = "trace", skip(self))]
    pub async fn add_route(
        &self,
        source: String,
        pattern: hardy_eid_pattern::EidPattern,
        action: routes::Action,
        priority: u32,
    ) {
        self.rib.add(pattern, source, action, priority).await
    }

    #[instrument(level = "trace", skip(self))]
    pub fn remove_route(
        &self,
        source: &str,
        pattern: &hardy_eid_pattern::EidPattern,
        action: &routes::Action,
        priority: u32,
    ) -> bool {
        self.rib.remove(pattern, source, action, priority)
    }
}
