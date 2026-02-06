use super::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Bpa {
    store: Arc<storage::Store>,
    rib: Arc<rib::Rib>,
    cla_registry: Arc<cla::registry::Registry>,
    service_registry: Arc<services::registry::ServiceRegistry>,
    filter_registry: Arc<filters::registry::Registry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Bpa {
    pub fn new(config: &config::Config) -> Self {
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

        // New Keys Registry (TODO: Make this load keys from the Config!)
        let keys_registry = Arc::new(keys::registry::Registry::new());

        let service_registry = Arc::new(services::registry::ServiceRegistry::new(
            config,
            rib.clone(),
        ));

        // New filter registry
        let filter_registry = Arc::new(filters::registry::Registry::new(config));

        // New dispatcher
        let dispatcher = Arc::new(dispatcher::Dispatcher::new(
            config,
            store.clone(),
            cla_registry.clone(),
            service_registry.clone(),
            rib.clone(),
            keys_registry,
            filter_registry.clone(),
        ));

        Self {
            store,
            rib,
            cla_registry,
            service_registry,
            filter_registry,
            dispatcher,
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub fn start(&self, recover_storage: bool) {
        if self.dispatcher.has_started() {
            warn!("BPA already started, ignoring start request");
            return;
        }

        // Start the dispatcher
        self.dispatcher.start();

        // Start the store
        self.store.start(self.dispatcher.clone(), recover_storage);

        // Start the RIB
        self.rib.start(self.dispatcher.clone());
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        self.dispatcher.shutdown().await;
        self.service_registry.shutdown().await;
        self.cla_registry.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;
        self.filter_registry.clear();
    }

    /// Register an Application (high-level, payload-only access)
    #[cfg_attr(feature = "tracing", instrument(skip(self, service)))]
    pub async fn register_application(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        if !self.dispatcher.has_started() {
            panic!("BPA must be started before registering applications");
        }

        self.service_registry
            .register_application(service_id, service, &self.dispatcher)
            .await
    }

    /// Register a low-level Service (full bundle access)
    #[cfg_attr(feature = "tracing", instrument(skip(self, service)))]
    pub async fn register_service(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn services::Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        if !self.dispatcher.has_started() {
            panic!("BPA must be started before registering services");
        }

        self.service_registry
            .register_service(service_id, service, &self.dispatcher)
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
        if !self.dispatcher.has_started() {
            panic!("BPA must be started before registering CLAs");
        }

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

    /// Register a filter at a hook point
    #[cfg_attr(feature = "tracing", instrument(skip(self, filter)))]
    pub fn register_filter(
        &self,
        hook: filters::Hook,
        name: &str,
        after: &[&str],
        filter: filters::Filter,
    ) -> Result<(), filters::Error> {
        self.filter_registry.register(hook, name, after, filter)
    }

    /// Unregister a filter by name from a hook point
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub fn unregister_filter(
        &self,
        hook: filters::Hook,
        name: &str,
    ) -> Result<Option<filters::Filter>, filters::Error> {
        self.filter_registry.unregister(hook, name)
    }
}
