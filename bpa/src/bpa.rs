use super::*;

/// Trait for registering CLAs, services, and routes with a BPA.
///
/// This trait abstracts the registration interface, allowing components
/// like CLAs and services to work with either a local [`Bpa`] instance
/// or a remote BPA via gRPC.
///
/// # For CLA Implementors
///
/// CLAs receive callbacks via the [`cla::Sink`] trait, which is provided
/// in [`cla::Cla::on_register`]. The Sink implementation differs based on
/// whether the BPA is local or remote, but the CLA code remains the same.
///
/// # For Service Implementors
///
/// Services receive callbacks via [`services::ServiceSink`] or
/// [`services::ApplicationSink`], provided in the respective `on_register`
/// methods.
#[async_trait]
pub trait BpaRegistration: Send + Sync {
    /// Register a Convergence Layer Adapter with the BPA.
    ///
    /// The CLA will receive a [`cla::Sink`] via [`cla::Cla::on_register`]
    /// for communicating back to the BPA.
    ///
    /// # Arguments
    ///
    /// * `name` - Unique name for this CLA instance
    /// * `address_type` - The address type this CLA handles (e.g., TCP)
    /// * `cla` - The CLA implementation
    /// * `policy` - Optional egress policy for traffic shaping
    ///
    /// # Returns
    ///
    /// The BPA's node IDs on success
    async fn register_cla(
        &self,
        name: String,
        address_type: Option<cla::ClaAddressType>,
        cla: Arc<dyn cla::Cla>,
        policy: Option<Arc<dyn policy::EgressPolicy>>,
    ) -> cla::Result<alloc::vec::Vec<hardy_bpv7::eid::NodeId>>;

    /// Register a low-level Service with full bundle access.
    ///
    /// The service will receive a [`services::ServiceSink`] via
    /// [`services::Service::on_register`] for sending bundles.
    ///
    /// # Arguments
    ///
    /// * `service_id` - Optional service identifier. If None, one is assigned.
    /// * `service` - The service implementation
    ///
    /// # Returns
    ///
    /// The endpoint ID assigned to this service
    async fn register_service(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn services::Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid>;

    /// Register a high-level Application with payload-only access.
    ///
    /// The application will receive an [`services::ApplicationSink`] via
    /// [`services::Application::on_register`] for sending payloads.
    ///
    /// # Arguments
    ///
    /// * `service_id` - Optional service identifier. If None, one is assigned.
    /// * `application` - The application implementation
    ///
    /// # Returns
    ///
    /// The endpoint ID assigned to this application
    async fn register_application(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        application: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid>;
}

pub struct Bpa {
    store: Arc<storage::Store>,
    rib: Arc<rib::Rib>,
    cla_registry: Arc<cla::registry::Registry>,
    service_registry: Arc<services::registry::Registry>,
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

        let service_registry = Arc::new(services::registry::Registry::new(config, rib.clone()));

        // New filter registry
        let filter_registry = Arc::new(filters::registry::Registry::new(config));

        // New dispatcher (returns Arc, starts immediately)
        let dispatcher = dispatcher::Dispatcher::new(
            config,
            store.clone(),
            cla_registry.clone(),
            service_registry.clone(),
            rib.clone(),
            keys_registry,
            filter_registry.clone(),
        );

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
        // Start the store
        self.store.start(self.dispatcher.clone(), recover_storage);

        // Start the RIB
        self.rib.start(self.dispatcher.clone());
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        // Shutdown order is critical for clean termination:
        //
        // 1. CLAs - Stop external bundle sources (network I/O)
        // 2. Services - Stop internal bundle sources (applications calling sink.send())
        // 3. Dispatcher - Drain remaining in-flight bundles (all sources now closed)
        // 4. RIB - No more routing lookups needed
        // 5. Store - No more data access needed
        //
        // CLAs and Services must shut down BEFORE dispatcher because they are
        // bundle sources. The dispatcher's processing pool may have tasks blocked
        // on CLA forwarding or waiting for service responses.

        self.cla_registry.shutdown().await;
        self.service_registry.shutdown().await;
        self.dispatcher.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;
        self.filter_registry.clear();
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

#[async_trait]
impl BpaRegistration for Bpa {
    /// Register an Application (high-level, payload-only access)
    #[cfg_attr(feature = "tracing", instrument(skip(self, service)))]
    async fn register_application(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_application(service_id, service, &self.dispatcher)
            .await
    }

    /// Register a low-level Service (full bundle access)
    #[cfg_attr(feature = "tracing", instrument(skip(self, service)))]
    async fn register_service(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn services::Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_service(service_id, service, &self.dispatcher)
            .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, cla, policy)))]
    async fn register_cla(
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
}
