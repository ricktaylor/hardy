use super::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Bpa {
    //store: Arc<store::Store>,
    rib: Arc<rib::Rib>,
    cla_registry: Arc<cla_registry::ClaRegistry>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Bpa {
    #[instrument]
    pub async fn start(config: &config::Config) -> Result<Self, Error> {
        trace!("Starting new BPA");

        if config.status_reports {
            warn!("Bundle status reports are enabled");
        }

        // New store
        let store = Arc::new(store::Store::new(config));

        // New RIB
        let rib = rib::Rib::new(config);

        // New registries
        let cla_registry = Arc::new(cla_registry::ClaRegistry::new(config, rib.clone()));
        let service_registry =
            Arc::new(service_registry::ServiceRegistry::new(config, rib.clone()));

        // Create a new dispatcher
        let dispatcher = Arc::new(dispatcher::Dispatcher::new(
            config,
            store.clone(),
            service_registry.clone(),
            rib.clone(),
        ));

        dispatcher.start()?;

        trace!("BPA started");

        Ok(Self {
            //store,
            rib,
            cla_registry,
            service_registry,
            dispatcher,
        })
    }

    #[instrument(skip(self))]
    pub async fn shutdown(&self) {
        trace!("Shutting down BPA");

        self.dispatcher.shutdown().await;
        self.service_registry.shutdown().await;
        self.cla_registry.shutdown().await;

        trace!("BPA stopped");
    }

    #[instrument(skip(self, service))]
    pub async fn register_service(
        &self,
        service_id: Option<service::ServiceId<'_>>,
        service: Arc<dyn service::Service>,
    ) -> service::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register(service_id, service, &self.dispatcher)
            .await
    }

    #[instrument(skip(self, cla))]
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

    #[instrument(skip(self))]
    pub fn add_route(
        &self,
        source: String,
        pattern: hardy_eid_pattern::EidPattern,
        action: routes::Action,
        priority: u32,
    ) {
        self.rib.add(pattern, source, action, priority)
    }

    #[instrument(skip(self))]
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
