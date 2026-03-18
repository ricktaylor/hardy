use core::num::NonZeroUsize;
use hardy_async::async_trait;
use hardy_bpv7::eid::{Eid, NodeId, Service as Bpv7Service};
use hardy_eid_patterns::EidPattern;

#[cfg(feature = "tracing")]
use crate::instrument;

use crate::cla::{Cla, ClaAddressType, ClaRegistry, Result as ClaResult};
use crate::dispatcher::Dispatcher;
use crate::filters::{Filter, FilterRegistry, Hook, Result as FilterResult};
use crate::keys::KeyRegistry;
use crate::policy::EgressPolicy;
use crate::rib::Rib;
use crate::routes::Action;
use crate::services::{Application, Result as ServiceResult, Service, ServiceRegistry};
use crate::storage::{BundleStorage, MetadataStorage, Store};
use crate::{Arc, BpaBuilder, BpaRegistration, NodeIds};

pub struct Bpa {
    store: Arc<Store>,
    rib: Arc<Rib>,
    cla_registry: Arc<ClaRegistry>,
    service_registry: Arc<ServiceRegistry>,
    filter_registry: Arc<FilterRegistry>,
    dispatcher: Arc<Dispatcher>,
}

impl Bpa {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        status_reports: bool,
        poll_channel_depth: NonZeroUsize,
        processing_pool_size: NonZeroUsize,
        lru_capacity: NonZeroUsize,
        max_cached_bundle_size: NonZeroUsize,
        node_ids: NodeIds,
        metadata_storage: Arc<dyn MetadataStorage>,
        bundle_storage: Arc<dyn BundleStorage>,
    ) -> Self {
        let store = Arc::new(Store::new(
            lru_capacity,
            max_cached_bundle_size,
            poll_channel_depth,
            metadata_storage,
            bundle_storage,
        ));

        let rib = Arc::new(Rib::new(node_ids.clone(), store.clone()));

        let cla_registry = Arc::new(ClaRegistry::new(
            (&node_ids).into(),
            poll_channel_depth.into(),
            rib.clone(),
            store.clone(),
        ));
        let keys_registry = Arc::new(KeyRegistry::new());
        let service_registry = Arc::new(ServiceRegistry::new(node_ids.clone(), rib.clone()));
        let filter_registry = Arc::new(FilterRegistry::new());

        // Auto-register RFC9171 validity filter unless disabled
        #[cfg(not(feature = "no-rfc9171-autoregister"))]
        {
            use crate::filters::rfc9171::Rfc9171ValidityFilter;

            filter_registry
                .register(
                    Hook::Ingress,
                    "rfc9171-validity",
                    &[],
                    Filter::Read(Arc::new(Rfc9171ValidityFilter::default())),
                )
                .expect("Failed to register RFC9171 validity filter");
        }

        let dispatcher = Dispatcher::new(
            status_reports,
            poll_channel_depth,
            processing_pool_size,
            node_ids,
            store.clone(),
            cla_registry.clone(),
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

    pub fn builder() -> BpaBuilder {
        BpaBuilder::default()
    }

    pub fn node_ids(&self) -> &NodeIds {
        self.dispatcher.node_ids()
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub fn start(&self, recover_storage: bool) {
        self.store.start(self.dispatcher.clone(), recover_storage);
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
        pattern: EidPattern,
        action: Action,
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
        pattern: &EidPattern,
        action: &Action,
        priority: u32,
    ) -> bool {
        self.rib.remove(pattern, source, action, priority).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, filter)))]
    pub fn register_filter(
        &self,
        hook: Hook,
        name: &str,
        after: &[&str],
        filter: Filter,
    ) -> FilterResult<()> {
        self.filter_registry.register(hook, name, after, filter)
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub fn unregister_filter(&self, hook: Hook, name: &str) -> FilterResult<Option<Filter>> {
        self.filter_registry.unregister(hook, name)
    }
}

#[async_trait]
impl BpaRegistration for Bpa {
    #[cfg_attr(feature = "tracing", instrument(skip(self, service)))]
    async fn register_application(
        &self,
        service_id: Option<Bpv7Service>,
        service: Arc<dyn Application>,
    ) -> ServiceResult<Eid> {
        self.service_registry
            .register_application(service_id, service, &self.dispatcher)
            .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, service)))]
    async fn register_service(
        &self,
        service_id: Option<Bpv7Service>,
        service: Arc<dyn Service>,
    ) -> ServiceResult<Eid> {
        self.service_registry
            .register_service(service_id, service, &self.dispatcher)
            .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, cla, policy)))]
    async fn register_cla(
        &self,
        name: String,
        address_type: Option<ClaAddressType>,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> ClaResult<Vec<NodeId>> {
        self.cla_registry
            .register(name, address_type, cla, &self.dispatcher, policy)
            .await
    }
}
