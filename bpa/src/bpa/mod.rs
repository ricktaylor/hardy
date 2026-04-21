mod admin;
mod lifecycle;
mod pipeline;

use hardy_async::async_trait;
use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
use hardy_bpv7::eid::NodeId;
#[cfg(feature = "instrument")]
use tracing::instrument;

use crate::Arc;
use crate::cla::registry::ClaRegistry;
use crate::cla::{self, Cla};
use crate::dispatcher::Dispatcher;
use crate::filters::registry::Registry as FilterRegistry;
use crate::filters::{self, Filter, Hook};
use crate::keys::registry::Registry as KeysRegistry;
use crate::node_ids::NodeIds;
use crate::policy::EgressPolicy;
use crate::registration::BpaRegistration;
use crate::rib::Rib;
use crate::routes::{self, RoutingAgent};
use crate::services::registry::ServiceRegistry;
use crate::services::{self, Service};
use crate::storage::Store;

/// The core Bundle Processing Agent (RFC 9171).
///
/// Holds references to the store, RIB, CLA/service/filter registries, and
/// dispatcher. Construct via [`BpaBuilder`](crate::builder::BpaBuilder).
///
/// After construction, call [`start()`](Bpa::start) to begin processing and
/// [`shutdown()`](Bpa::shutdown) for ordered teardown.
pub struct Bpa {
    pub(crate) node_ids: Arc<NodeIds>,
    pub(crate) store: Arc<Store>,
    pub(crate) rib: Arc<Rib>,
    pub(crate) cla_registry: Arc<ClaRegistry>,
    pub(crate) service_registry: Arc<ServiceRegistry>,
    pub(crate) filter_registry: Arc<FilterRegistry>,
    pub(crate) keys_registry: Arc<KeysRegistry>,
    pub(crate) processing_pool: hardy_async::BoundedTaskPool,
    pub(crate) status_reports: bool,
    pub(crate) dispatcher: Arc<Dispatcher>,
}

impl Bpa {
    pub(crate) fn from_parts(
        node_ids: Arc<NodeIds>,
        store: Arc<Store>,
        rib: Arc<Rib>,
        cla_registry: Arc<ClaRegistry>,
        service_registry: Arc<ServiceRegistry>,
        filter_registry: Arc<FilterRegistry>,
        keys_registry: Arc<KeysRegistry>,
        processing_pool: hardy_async::BoundedTaskPool,
        status_reports: bool,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            node_ids,
            store,
            rib,
            cla_registry,
            service_registry,
            filter_registry,
            keys_registry,
            processing_pool,
            status_reports,
            dispatcher,
        }
    }

    /// Build a key provider closure for CBOR parsing and BPSec operations.
    pub(crate) fn key_provider(&self) -> impl Fn(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource> + Clone {
        let keys_registry = self.keys_registry.clone();
        move |bundle, data| keys_registry.key_source(bundle, data)
    }

    /// Register a filter at a hook point.
    #[cfg_attr(feature = "instrument", instrument(skip(self, filter)))]
    pub fn register_filter(
        &self,
        hook: Hook,
        name: &str,
        after: &[&str],
        filter: Filter,
    ) -> Result<(), filters::Error> {
        self.filter_registry.register(hook, name, after, filter)
    }

    /// Unregister a filter by name from a hook point.
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn unregister_filter(
        &self,
        hook: Hook,
        name: &str,
    ) -> Result<Option<Filter>, filters::Error> {
        self.filter_registry.unregister(hook, name)
    }
}

#[async_trait]
impl BpaRegistration for Bpa {
    async fn register_cla(
        &self,
        name: String,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        self.cla_registry
            .register(name, cla, &self.dispatcher, policy)
            .await
    }

    async fn register_service(
        &self,
        service_id: hardy_bpv7::eid::Service,
        service: Arc<dyn Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_service(
                service_id,
                service,
                &self.node_ids,
                &self.rib,
                &self.dispatcher,
            )
            .await
    }

    async fn register_application(
        &self,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_application(
                service_id,
                application,
                &self.node_ids,
                &self.rib,
                &self.dispatcher,
            )
            .await
    }

    async fn register_dynamic_service(
        &self,
        service: Arc<dyn Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_dynamic_service(service, &self.node_ids, &self.rib, &self.dispatcher)
            .await
    }

    async fn register_dynamic_application(
        &self,
        application: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_dynamic_application(application, &self.node_ids, &self.rib, &self.dispatcher)
            .await
    }

    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn RoutingAgent>,
    ) -> routes::Result<Vec<NodeId>> {
        self.rib.register_agent(name, agent).await
    }
}
