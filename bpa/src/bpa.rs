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
    pub async fn start(config: &config::Config) -> Self {
        trace!("Starting new BPA");

        // New store
        let store = Arc::new(store::Store::new(config));

        // New RIB
        let rib = rib::Rib::new();

        // New registries
        let cla_registry = Arc::new(cla_registry::ClaRegistry::new(rib.clone()));
        let service_registry = Arc::new(service_registry::ServiceRegistry::new(config));

        // Create a new dispatcher
        let dispatcher = dispatcher::Dispatcher::new(
            config,
            store.clone(),
            service_registry.clone(),
            rib.clone(),
            cla_registry.clone(),
        );

        trace!("BPA started");

        Self {
            //store,
            rib,
            cla_registry,
            service_registry,
            dispatcher,
        }
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
        eid: Option<&service::ServiceName<'_>>,
        service: Arc<dyn service::Service>,
    ) -> service::Result<()> {
        self.service_registry
            .register(eid, service, self.dispatcher.clone())
            .await
    }

    #[instrument(skip(self, cla))]
    pub async fn register_cla(
        &self,
        ident_prefix: &str,
        cla: Arc<dyn cla::Cla>,
    ) -> cla::Result<String> {
        self.cla_registry
            .register(ident_prefix, cla, self.dispatcher.clone())
            .await
    }

    pub async fn add_forwarding_action(
        &self,
        source: String,
        pattern: eid_pattern::EidPattern,
        action: routes::Action,
        priority: u32,
    ) {
        self.rib.add(pattern, source, action, priority).await
    }

    pub async fn remove_forwarding_action(
        &self,
        source: &str,
        pattern: &eid_pattern::EidPattern,
        action: &routes::Action,
        priority: u32,
    ) -> bool {
        self.rib.remove(pattern, source, action, priority).await
    }
}
