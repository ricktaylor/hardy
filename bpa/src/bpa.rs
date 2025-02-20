use super::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Config {
    pub status_reports: bool,
    pub wait_sample_interval: time::Duration,
    pub max_forwarding_delay: u32,
    pub metadata_storage: Option<Arc<dyn storage::MetadataStorage>>,
    pub bundle_storage: Option<Arc<dyn storage::BundleStorage>>,
    pub admin_endpoints: Vec<bpv7::Eid>,
    pub ipn_2_element: Option<bpv7::EidPatternMap<(), ()>>,
}

impl std::default::Default for Config {
    fn default() -> Self {
        Self {
            status_reports: false,
            wait_sample_interval: time::Duration::seconds(60),
            max_forwarding_delay: 5,
            metadata_storage: None,
            bundle_storage: None,
            admin_endpoints: Vec::new(),
            ipn_2_element: None,
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("status_reports", &self.status_reports)
            .field("wait_sample_interval", &self.wait_sample_interval)
            .field("max_forwarding_delay", &self.max_forwarding_delay)
            .field("admin_endpoints", &self.admin_endpoints)
            .field("ipn_2_element", &self.ipn_2_element)
            .finish()
    }
}

pub struct Bpa {
    //store: Arc<store::Store>,
    fib: Arc<fib_impl::Fib>,
    cla_registry: Arc<cla_registry::ClaRegistry>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Bpa {
    #[instrument]
    pub async fn start(config: Config) -> Self {
        trace!("Starting new BPA");

        // Build admin endpoints
        let admin_endpoints = Arc::new(admin_endpoints::AdminEndpoints::new(
            &config.admin_endpoints,
        ));

        // New store
        let store = Arc::new(store::Store::new(&config));

        // New FIB
        let fib = Arc::new(fib_impl::Fib::new());

        // New registries
        let cla_registry = Arc::new(cla_registry::ClaRegistry::new(fib.clone()));
        let service_registry = Arc::new(service_registry::ServiceRegistry::new(
            admin_endpoints.clone(),
        ));

        // Create a new dispatcher
        let (dispatcher, rx) = dispatcher::Dispatcher::new(
            &config,
            store.clone(),
            admin_endpoints,
            service_registry.clone(),
            fib.clone(),
        );
        let dispatcher = Arc::new(dispatcher);

        // Spawn the dispatch task
        tokio::spawn(dispatcher::Dispatcher::run(dispatcher.clone(), rx));

        trace!("BPA started");

        Self {
            //store,
            fib,
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
        ident: &str,
        kind: &str,
        cla: Arc<dyn cla::Cla>,
    ) -> cla::Result<()> {
        self.cla_registry
            .register(ident, kind, cla, self.dispatcher.clone())
            .await
    }

    pub async fn add_forwarding_action(
        &self,
        id: &str,
        pattern: &bpv7::EidPattern,
        action: &fib::Action,
        priority: u32,
    ) -> fib::Result<()> {
        self.fib.add(id, pattern, action, priority).await
    }

    pub async fn remove_forwarding_action(&self, id: &str, pattern: &bpv7::EidPattern) -> usize {
        self.fib.remove(id, pattern).await
    }
}
