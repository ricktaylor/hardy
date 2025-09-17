use super::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Bpa {
    store: Arc<store::Store>,
    reaper: Arc<reaper::Reaper>,
    rib: Arc<rib::Rib>,
    cla_registry: Arc<cla::registry::Registry>,
    service_registry: Arc<service_registry::ServiceRegistry>,
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
        let store = Arc::new(store::Store::new(config));

        // New RIB
        let rib = Arc::new(rib::Rib::new(config));

        // New registries
        let cla_registry = Arc::new(cla::registry::Registry::new(config, rib.clone()));
        let service_registry =
            Arc::new(service_registry::ServiceRegistry::new(config, rib.clone()));

        // New reaper
        let reaper = Arc::new(reaper::Reaper::new(store.clone()));

        // New dispatcher
        let dispatcher = Arc::new(dispatcher::Dispatcher::new(
            config,
            store.clone(),
            reaper.clone(),
            service_registry.clone(),
            rib.clone(),
        ));

        // Start the reaper
        reaper.start(dispatcher.clone());

        // And finally restart the store
        store.start(dispatcher.clone(), recover_storage);

        info!("BPA started");

        Ok(Self {
            store,
            reaper,
            rib,
            cla_registry,
            service_registry,
            dispatcher,
        })
    }

    // TODO: Make this a Drop impl
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        trace!("Shutting down BPA");

        self.dispatcher.shutdown().await;
        self.service_registry.shutdown().await;
        self.cla_registry.shutdown().await;
        self.reaper.shutdown().await;
        self.store.shutdown().await;

        trace!("BPA stopped");
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, service)))]
    pub async fn register_service(
        &self,
        service_id: Option<service::ServiceId<'_>>,
        service: Arc<dyn service::Service>,
    ) -> service::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register(service_id, service, &self.dispatcher)
            .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, cla)))]
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

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn add_route(
        &self,
        source: String,
        pattern: hardy_eid_pattern::EidPattern,
        action: routes::Action,
        priority: u32,
    ) {
        self.rib.add(pattern, source, action, priority).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
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
