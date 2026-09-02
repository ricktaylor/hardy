use core::time::Duration;

use hardy_async::async_trait;
use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use time::OffsetDateTime;
use tracing::{error, info};

use crate::{
    Arc, HashMap, Weak, dispatcher, node_ids, routing,
    services::{self, StatusNotify},
    stream::{CancellableReceiver, Receiver, Segment},
};

// ServiceRegistry uses hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. RNG for auto-generated IDs is called OUTSIDE the lock
// 3. Lock only protects contains_key + insert (both O(1))
// 4. No blocking/sleeping while holding lock

/// Distinguishes between low-level Service and high-level Application registrations
pub enum ServiceImpl {
    /// Low-level service with full bundle access
    LowLevel(Arc<dyn services::Service>),
    /// High-level application receiving only payload
    Application(Arc<dyn services::Application>),
}

pub struct Service {
    pub service: ServiceImpl,
    pub service_id: hardy_bpv7::eid::Service,
    // Cancelled at unregistration; every in-flight stream of this
    // registration races it and fails immediately.
    pub(crate) cancel: hardy_async::CancellationToken,
}

impl Service {
    pub async fn on_status_notify(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        match &self.service {
            ServiceImpl::LowLevel(svc) => {
                svc.on_status_notify(bundle_id, from, kind, reason, timestamp)
                    .await
            }
            ServiceImpl::Application(app) => {
                app.on_status_notify(bundle_id, from, kind, reason, timestamp)
                    .await
            }
        }
    }
}

impl PartialEq for Service {
    fn eq(&self, other: &Self) -> bool {
        self.service_id == other.service_id
    }
}

impl Eq for Service {}

impl PartialOrd for Service {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Service {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.service_id.cmp(&other.service_id)
    }
}

impl core::hash::Hash for Service {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.service_id.hash(state);
    }
}

impl core::fmt::Debug for Service {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Service")
            .field("eid", &self.service_id)
            .finish_non_exhaustive()
    }
}

/// Sink implementation for both Service and Application traits
struct Sink {
    service: Weak<Service>,
    /// Full EID for this service (pre-resolved at activation time)
    eid: Eid,
    registry: Arc<ServiceRegistry>,
    node_ids: Arc<node_ids::NodeIds>,
    rib: Arc<routing::Rib>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Sink {
    async fn unregister_inner(&self) {
        if let Some(service) = self.service.upgrade()
            && let Err(e) = self
                .registry
                .unregister(service, &self.node_ids, &self.rib)
                .await
        {
            error!("Failed to unregister service: {e}");
        }
    }

    // A component that unregisters mid-send must not originate its
    // bundle: the registration's token races every pull, so teardown
    // wakes the wrapped stream immediately — even parked behind a
    // stalled producer — and the send surfaces as cancelled. Sink-side,
    // so the dispatcher's stream consumers stay registration-agnostic.
    fn cancellable<'a>(
        &self,
        stream: &'a mut dyn Receiver<Segment>,
    ) -> services::Result<CancellableReceiver<'a, Segment>> {
        let service = self
            .service
            .upgrade()
            .ok_or(services::Error::Disconnected)?;
        Ok(CancellableReceiver {
            inner: stream,
            token: service.cancel.clone(),
        })
    }
}

#[async_trait]
impl services::ServiceSink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(&self, stream: &mut dyn Receiver<Segment>) -> services::Result<Id> {
        let mut stream = self.cancellable(stream)?;
        self.dispatcher
            .local_dispatch_raw(&self.eid, &mut stream)
            .await
    }
}

#[async_trait]
impl services::ApplicationSink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(
        &self,
        destination: Eid,
        lifetime: Duration,
        options: Option<services::SendOptions>,
        size_hint: Option<u64>,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<Id> {
        let mut stream = self.cancellable(stream)?;
        self.dispatcher
            .local_dispatch(
                self.eid.clone(),
                destination,
                lifetime,
                options,
                size_hint,
                &mut stream,
            )
            .await
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(service) = self.service.upgrade() {
            let registry = self.registry.clone();
            let node_ids = self.node_ids.clone();
            let rib = self.rib.clone();
            hardy_async::spawn!(self.registry.tasks, "sink_drop_cleanup", async move {
                if let Err(e) = registry.unregister(service, &node_ids, &rib).await {
                    error!("Failed to unregister service: {e}");
                }
            });
        }
    }
}

type ServiceMap = HashMap<hardy_bpv7::eid::Service, Arc<Service>>;

/// Inserts a fresh [`Service`] into `services`, rejecting a duplicate id.
pub(crate) struct ServiceRegistryBuilder {
    services: ServiceMap,
}

impl ServiceRegistryBuilder {
    pub fn new() -> Self {
        Self {
            services: Default::default(),
        }
    }

    pub fn insert(
        &mut self,
        service_id: hardy_bpv7::eid::Service,
        service: ServiceImpl,
    ) -> services::Result<()> {
        if self.services.contains_key(&service_id) {
            return Err(services::Error::ServiceIdInUse(service_id.to_string()));
        }
        let service = Arc::new(Service {
            service,
            service_id: service_id.clone(),
            cancel: hardy_async::CancellationToken::new(),
        });
        self.services.insert(service_id.clone(), service);
        info!("Inserted service: {service_id}");
        Ok(())
    }

    pub async fn build(
        self,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Arc<ServiceRegistry>> {
        let registry = Arc::new(ServiceRegistry {
            services: hardy_async::sync::spin::Mutex::new(self.services),
            next_dynamic: core::sync::atomic::AtomicU32::new(DYNAMIC_SERVICE_BASE),
            tasks: hardy_async::TaskPool::new(),
        });

        let ids: Vec<_> = registry.services.lock().keys().cloned().collect();
        for id in ids {
            registry.register(&id, node_ids, rib, dispatcher).await?;
        }

        Ok(registry)
    }
}

/// Base for dynamically assigned IPN service numbers.
/// Starts high to avoid collisions with explicitly assigned IDs.
const DYNAMIC_SERVICE_BASE: u32 = 0x8000_0000;

pub(crate) struct ServiceRegistry {
    services: hardy_async::sync::spin::Mutex<ServiceMap>,
    next_dynamic: core::sync::atomic::AtomicU32,
    tasks: hardy_async::TaskPool,
}

impl ServiceRegistry {
    pub async fn shutdown(&self, node_ids: &node_ids::NodeIds, rib: &Arc<routing::Rib>) {
        let services = self
            .services
            .lock()
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>();

        if !services.is_empty() {
            metrics::gauge!("bpa.service.registered").decrement(services.len() as f64);
        }

        for service in services {
            if let Err(e) = self.unregister_service(service, node_ids, rib).await {
                error!("Failed to unregister service: {e}");
            }
        }

        self.tasks.shutdown().await;
    }

    pub async fn register_service(
        self: &Arc<Self>,
        service_id: hardy_bpv7::eid::Service,
        service: Arc<dyn services::Service>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        self.insert_inner(service_id.clone(), ServiceImpl::LowLevel(service))?;
        self.register(&service_id, node_ids, rib, dispatcher).await
    }

    pub async fn register_application(
        self: &Arc<Self>,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn services::Application>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        self.insert_inner(service_id.clone(), ServiceImpl::Application(application))?;
        self.register(&service_id, node_ids, rib, dispatcher).await
    }

    /// Register a service with a dynamically assigned IPN service number.
    pub async fn register_dynamic_service(
        self: &Arc<Self>,
        service: Arc<dyn services::Service>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        let service_id = self.allocate_dynamic_id();
        self.register_service(service_id, service, node_ids, rib, dispatcher)
            .await
    }

    /// Register an application with a dynamically assigned IPN service number.
    pub async fn register_dynamic_application(
        self: &Arc<Self>,
        application: Arc<dyn services::Application>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        let service_id = self.allocate_dynamic_id();
        self.register_application(service_id, application, node_ids, rib, dispatcher)
            .await
    }

    fn allocate_dynamic_id(&self) -> hardy_bpv7::eid::Service {
        let id = self
            .next_dynamic
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        hardy_bpv7::eid::Service::Ipn(id)
    }

    fn insert_inner(
        &self,
        service_id: hardy_bpv7::eid::Service,
        service: ServiceImpl,
    ) -> services::Result<()> {
        let mut services = self.services.lock();
        if services.contains_key(&service_id) {
            return Err(services::Error::ServiceIdInUse(service_id.to_string()));
        }
        let service = Arc::new(Service {
            service,
            service_id: service_id.clone(),
            cancel: hardy_async::CancellationToken::new(),
        });
        services.insert(service_id, service);
        Ok(())
    }

    async fn register(
        self: &Arc<Self>,
        service_id: &hardy_bpv7::eid::Service,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        let service = self.services.lock().get(service_id).cloned().unwrap();
        let eid = node_ids.resolve_eid(service_id)?;

        let _ = rib.add_service(eid.clone(), service.clone()).await;

        let sink = Sink {
            service: Arc::downgrade(&service),
            eid: eid.clone(),
            registry: self.clone(),
            node_ids: Arc::new(node_ids.clone()),
            rib: rib.clone(),
            dispatcher: dispatcher.clone(),
        };
        match &service.service {
            ServiceImpl::LowLevel(s) => s.on_register(&eid, Box::new(sink)).await,
            ServiceImpl::Application(a) => a.on_register(&eid, Box::new(sink)).await,
        }

        // Re-announce parked bundles off the registration path: an
        // announcement can block on the component (a remote session's
        // bounded event buffer drains only after registration returns),
        // so awaiting the poll here could deadlock registration against
        // its own announcements. The poll's status CAS keeps
        // overlapping polls safe.
        {
            let dispatcher = dispatcher.clone();
            let eid = eid.clone();
            hardy_async::spawn!(self.tasks, "poll_service_waiting", async move {
                dispatcher.poll_service_waiting(&eid).await;
            });
        }

        metrics::gauge!("bpa.service.registered").increment(1.0);
        Ok(eid)
    }

    async fn unregister(
        &self,
        service: Arc<Service>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
    ) -> services::Result<()> {
        let service = self.services.lock().remove(&service.service_id);

        if let Some(service) = service {
            metrics::gauge!("bpa.service.registered").decrement(1.0);
            self.unregister_service(service, node_ids, rib).await?;
        }
        Ok(())
    }

    async fn unregister_service(
        &self,
        service: Arc<Service>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
    ) -> services::Result<()> {
        // First: wake every in-flight stream of this registration.
        service.cancel.cancel();

        let eid = node_ids.resolve_eid(&service.service_id)?;
        rib.remove_service(&eid, service.clone()).await;

        match &service.service {
            ServiceImpl::LowLevel(svc) => svc.on_unregister().await,
            ServiceImpl::Application(app) => app.on_unregister().await,
        }

        info!("Unregistered service: {eid}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::bpa::{Bpa, BpaRegistration};

    struct TestApp {
        sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    }

    impl TestApp {
        fn new() -> Self {
            Self {
                sink: hardy_async::sync::spin::Once::new(),
            }
        }
    }

    use super::*;

    #[async_trait]
    impl services::Application for TestApp {
        async fn on_register(
            &self,
            _source: &hardy_bpv7::eid::Eid,
            sink: Box<dyn services::ApplicationSink>,
        ) {
            self.sink.call_once(|| sink);
        }
        async fn on_unregister(&self) {}
        async fn on_deliver(
            &self,
            _bundle_id: &Id,
            _expiry: OffsetDateTime,
            _ack_requested: bool,
            _adu_size: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            Ok(())
        }
        async fn on_status_notify(
            &self,
            _bundle_id: &Id,
            _from: &hardy_bpv7::eid::Eid,
            _kind: services::StatusNotify,
            _reason: ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
        }
    }

    // Registering two applications with the same explicit IPN service number should fail.
    #[tokio::test]
    async fn test_duplicate_reg() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false);

        let svc_id = hardy_bpv7::eid::Service::Ipn(42);

        // First registration should succeed
        let app1 = Arc::new(TestApp::new());
        let result = bpa.register_application(svc_id.clone(), app1).await;
        assert!(result.is_ok(), "First registration should succeed");

        // Second registration with the same service number should fail
        let app2 = Arc::new(TestApp::new());
        let result = bpa.register_application(svc_id, app2).await;
        assert!(
            matches!(result, Err(services::Error::ServiceIdInUse(ref id)) if id == "42"),
            "Duplicate registration should return ServiceIdInUse, got: {result:?}"
        );

        bpa.shutdown().await;
    }

    // After an application drops its sink (unregisters), the service ID should be freed
    // for re-registration.
    #[tokio::test]
    async fn test_cleanup() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false);

        let svc_id = hardy_bpv7::eid::Service::Ipn(99);

        // Register
        let app1 = Arc::new(TestApp::new());
        let result = bpa.register_application(svc_id.clone(), app1.clone()).await;
        assert!(result.is_ok());

        // Unregister via the sink
        app1.sink
            .get()
            .expect("Sink should be set")
            .unregister()
            .await;

        // Small yield to let the unregister propagate
        tokio::task::yield_now().await;

        // Re-registration with the same service number should now succeed
        let app2 = Arc::new(TestApp::new());
        let result = bpa.register_application(svc_id, app2).await;
        assert!(
            result.is_ok(),
            "Re-registration after cleanup should succeed, got: {result:?}"
        );

        bpa.shutdown().await;
    }
}
