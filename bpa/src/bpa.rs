use super::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Bpa {
    store: Arc<storage::Store>,
    rib: Arc<rib::Rib>,
    cla_registry: Arc<cla::registry::Registry>,
    service_registry: Arc<services::registry::ServiceRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Bpa {
    #[cfg_attr(feature = "tracing", instrument)]
    pub async fn start(config: &config::Config, recover_storage: bool) -> Result<Self, Error> {
        info!("Starting new BPA");

        if config.status_reports {
            warn!("Bundle status reports are enabled");
        }

        // New store
        let store = Arc::new(storage::Store::new(config));

        // New RIB
        let rib = Arc::new(rib::Rib::new(config, store.clone()));

        // New registries
        let cla_registry = Arc::new(cla::registry::Registry::new(
            config,
            rib.clone(),
            store.clone(),
        ));
        let service_registry = Arc::new(services::registry::ServiceRegistry::new(
            config,
            rib.clone(),
        ));

        // New Keys Registry (TODO: Make this laod keys from the Config!)
        let keys_registry = Arc::new(keys::registry::Registry::new());

        // New dispatcher
        let dispatcher = Arc::new(dispatcher::Dispatcher::new(
            config,
            store.clone(),
            cla_registry.clone(),
            service_registry.clone(),
            rib.clone(),
            keys_registry,
        ));

        // Start the store
        store.start(dispatcher.clone(), recover_storage);

        // Start the RIB
        rib.start(dispatcher.clone());

        info!("BPA started");

        Ok(Self {
            store,
            rib,
            cla_registry,
            service_registry,
            dispatcher,
        })
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        info!("Shutting down BPA");

        self.dispatcher.shutdown().await;
        self.service_registry.shutdown().await;
        self.cla_registry.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;

        info!("BPA stopped");
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, service)))]
    pub async fn register_service(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register(service_id, service, &self.dispatcher)
            .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, cla, policy)))]
    pub async fn register_cla(
        &self,
        name: String,
        address_type: Option<cla::ClaAddressType>,
        cla: Arc<dyn cla::Cla>,
        policy: Option<Arc<dyn policy::EgressPolicy>>,
    ) -> cla::Result<Vec<hardy_bpv7::eid::NodeId>> {
        self.cla_registry
            .register(name, address_type, cla, &self.dispatcher, policy)
            .await
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(skip(self, pattern, action), fields(pattern = %pattern, action = %action))
    )]
    pub async fn add_route(
        &self,
        source: String,
        pattern: hardy_eid_patterns::EidPattern,
        action: routes::Action,
        priority: u32,
    ) -> bool {
        self.rib.add(pattern, source, action, priority).await
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(skip(self, pattern, action), fields(pattern = %pattern, action = %action))
    )]
    pub async fn remove_route(
        &self,
        source: &str,
        pattern: &hardy_eid_patterns::EidPattern,
        action: &routes::Action,
        priority: u32,
    ) -> bool {
        self.rib.remove(pattern, source, action, priority).await
    }
}
