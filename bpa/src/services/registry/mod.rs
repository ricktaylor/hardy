use hardy_async::{async_trait, sync::spin::Once};
use hardy_bpv7::eid::Eid;
use portable_atomic::{AtomicU32, Ordering};
use tracing::{error, info};

pub use self::record::{Service, ServiceImpl};
use self::sink::{Sink, SinkHandle};
use crate::{
    Arc, Bytes, Weak, bundle,
    claim_table::ClaimTable,
    dispatcher, node_ids, routing,
    services::{self, ApplicationContext, ServiceContext, factory::BoxFactory},
    storage,
};

mod record;
mod sink;

// Runs a factory at its resolved endpoint, handing it the shared sink.
fn build(factory: BoxFactory, sink: &Arc<Sink>, eid: &Eid) -> services::Result<ServiceImpl> {
    Ok(match factory {
        BoxFactory::Service(build) => {
            ServiceImpl::LowLevel(build(ServiceContext::new(sink.clone(), eid.clone()))?)
        }
        BoxFactory::Application(build) => {
            ServiceImpl::Application(build(ApplicationContext::new(sink.clone(), eid.clone()))?)
        }
    })
}

// A configured service or application, built when the registry starts.
type PendingService = (hardy_bpv7::eid::Service, BoxFactory);

pub struct ServiceRegistryBuilder {
    services: Vec<PendingService>,
}

impl ServiceRegistryBuilder {
    pub fn new() -> Self {
        Self {
            services: Default::default(),
        }
    }

    // Parks a configured registration, refusing every service id that
    // activation would: the administrative endpoint, one already taken, and
    // one whose scheme has no node id. Activation happens in
    // [`ServiceRegistry::start`], after storage recovery — registrations
    // must not race the consistency check — so configuration errors are
    // surfaced here, where the build can fail.
    pub fn insert(
        &mut self,
        service_id: hardy_bpv7::eid::Service,
        factory: BoxFactory,
        node_ids: &node_ids::NodeIds,
    ) -> services::Result<()> {
        if node_ids::service_is_admin_endpoint(&service_id) {
            return Err(services::Error::AdministrativeEndpoint(
                service_id.to_string(),
            ));
        }
        if self.services.iter().any(|(id, _)| *id == service_id) {
            return Err(services::Error::ServiceIdInUse(service_id.to_string()));
        }
        node_ids.resolve_eid(&service_id)?;
        info!("Inserted service: {service_id}");
        self.services.push((service_id, factory));
        Ok(())
    }

    // Constructs the registry with the configured registrations parked.
    pub fn build(self) -> Arc<ServiceRegistry> {
        Arc::new(ServiceRegistry {
            services: ClaimTable::new(),
            pending: hardy_async::sync::spin::Mutex::new(self.services),
            next_dynamic: AtomicU32::new(DYNAMIC_SERVICE_BASE),
            tasks: hardy_async::TaskPool::new(),
        })
    }
}

/// Base for dynamically assigned IPN service numbers.
/// Starts high to avoid collisions with explicitly assigned IDs.
const DYNAMIC_SERVICE_BASE: u32 = 0x8000_0000;

pub struct ServiceRegistry {
    services: ClaimTable<hardy_bpv7::eid::Service, Arc<Service>>,
    // Builder-configured registrations, parked until `start` builds and
    // activates them after storage recovery.
    pending: hardy_async::sync::spin::Mutex<Vec<PendingService>>,
    next_dynamic: AtomicU32,
    tasks: hardy_async::TaskPool,
}

impl ServiceRegistry {
    /// Build and activate the builder-configured registrations, in
    /// configuration order. Called by `Bpa::start` once storage recovery
    /// has completed: a registration going live earlier would race the
    /// consistency check.
    pub async fn start(
        self: &Arc<Self>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) {
        let pending = core::mem::take(&mut *self.pending.lock());
        for (service_id, factory) in pending {
            // The service id was validated at build, so a failure here is a
            // factory refusing its endpoint, or an id a runtime registration
            // claimed first.
            if let Err(e) = self
                .register(&service_id, factory, node_ids, rib, dispatcher)
                .await
            {
                error!("Failed to activate configured service {service_id}: {e}");
            }
        }
    }
    pub async fn shutdown(&self, rib: &Arc<routing::Rib>) {
        let services = self.services.drain();

        if !services.is_empty() {
            metrics::gauge!("bpa.service.registered").decrement(services.len() as f64);
        }

        for (_, service) in services {
            if let Err(e) = self.unregister_service(service, rib).await {
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
        self.register(
            &service_id,
            BoxFactory::service(service),
            node_ids,
            rib,
            dispatcher,
        )
        .await
    }

    pub async fn register_application(
        self: &Arc<Self>,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn services::Application>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        self.register(
            &service_id,
            BoxFactory::application(application),
            node_ids,
            rib,
            dispatcher,
        )
        .await
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
        let id = self.next_dynamic.fetch_add(1, Ordering::Relaxed);
        hardy_bpv7::eid::Service::Ipn(id)
    }

    // Claims the service id, builds the component at its resolved
    // endpoint, publishes it, and activates it. Configured and runtime
    // registrations all arrive here.
    //
    // The id is claimed before the factory runs, so a taken id is refused
    // before anything is built, and the factory runs outside the registry's
    // lock.
    async fn register(
        self: &Arc<Self>,
        service_id: &hardy_bpv7::eid::Service,
        factory: BoxFactory,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        if node_ids::service_is_admin_endpoint(service_id) {
            return Err(services::Error::AdministrativeEndpoint(
                service_id.to_string(),
            ));
        }
        let eid = node_ids.resolve_eid(service_id)?;
        if !self.services.claim(service_id.clone()) {
            return Err(services::Error::ServiceIdInUse(service_id.to_string()));
        }

        let sink = Arc::new(Sink {
            service: Once::new(),
            eid: eid.clone(),
            registry: self.clone(),
            rib: rib.clone(),
            dispatcher: dispatcher.clone(),
        });
        let service = match build(factory, &sink, &eid) {
            Ok(service) => service,
            Err(e) => {
                self.services.release(service_id);
                return Err(e);
            }
        };
        let service = self.publish(service, service_id, &eid, dispatcher)?;
        sink.service.call_once(|| Arc::downgrade(&service));

        let _ = rib.add_service(eid.clone(), service.clone()).await;

        let handle = Box::new(SinkHandle(sink));
        match &service.service {
            ServiceImpl::LowLevel(s) => s.on_register(&eid, handle).await,
            ServiceImpl::Application(a) => a.on_register(&eid, handle).await,
        }
        // The post-registration poll is spawned, never awaited inline: a
        // sink whose event buffer drains only after registration returns —
        // the gRPC-bridge shape — would deadlock registration against its
        // own announcements. Overlapping polls are safe: the poll claims
        // each bundle out of WaitingForService with a status CAS.
        {
            let dispatcher = dispatcher.clone();
            let eid = eid.clone();
            hardy_async::spawn!(self.tasks, "post_registration_poll", async move {
                dispatcher.poll_service_waiting(&eid).await;
            });
        }
        metrics::gauge!("bpa.service.registered").increment(1.0);
        Ok(eid)
    }

    // Constructs the service complete — the delivery queue and canonical
    // EID are constructor arguments — then files it against the claim on
    // its id: nothing reachable is ever half-built, and the RIB only ever
    // sees a deliverable service. The channel's creation-time poll recovers
    // any DeliverPending bundles left over from a previous registration
    // under the same EID.
    fn publish(
        &self,
        service: ServiceImpl,
        service_id: &hardy_bpv7::eid::Service,
        eid: &Eid,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Arc<Service>> {
        let (tx, rx) = dispatcher.new_delivery_channel(eid);
        let service = Service::new(service, service_id.clone(), eid.clone(), tx);
        if !self.services.file(service_id, service.clone()) {
            // A shutdown ran while the factory did and took the claim with
            // it. The service was never reached; close the channel it
            // opened, because the channel spawns its storage poller at
            // creation and dropping the sender does not close it.
            service.close_queue();
            return Err(services::Error::Disconnected);
        }
        dispatcher.start_delivery_queue(service.clone(), rx);
        Ok(service)
    }

    async fn unregister(
        &self,
        service: Arc<Service>,
        rib: &Arc<routing::Rib>,
    ) -> services::Result<()> {
        // Only this registration: a stale sink must not take a later
        // registration under the same id.
        let service = self
            .services
            .remove_if(&service.service_id, |filed| Arc::ptr_eq(filed, &service));

        if let Some(service) = service {
            metrics::gauge!("bpa.service.registered").decrement(1.0);
            self.unregister_service(service, rib).await?;
        }
        Ok(())
    }

    async fn unregister_service(
        &self,
        service: Arc<Service>,
        rib: &Arc<routing::Rib>,
    ) -> services::Result<()> {
        // First: wake every in-flight stream of this registration.
        service.cancel.cancel();

        // Close the delivery queue: the consumer finishes its in-flight
        // delivery and exits; queued bundles are swept to WaitingForService
        // by the RIB removal below.
        service.close_queue();

        let eid = service.eid().clone();
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
            _bundle_id: &hardy_bpv7::bundle::Id,
            _expiry: time::OffsetDateTime,
            _ack_requested: bool,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
        ) -> services::Result<()> {
            Ok(())
        }
        async fn on_status_notify(
            &self,
            _bundle_id: &hardy_bpv7::bundle::Id,
            _from: &hardy_bpv7::eid::Eid,
            _kind: services::StatusNotify,
            _reason: hardy_bpv7::status_report::ReasonCode,
            _timestamp: Option<time::OffsetDateTime>,
        ) {
        }
    }

    // Registering two applications with the same explicit IPN service number should fail.
    #[tokio::test]
    async fn test_duplicate_reg() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;

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
        bpa.start(false).await;

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

    // A minimal low-level service: registration-shape tests never exercise
    // delivery.
    struct TestService;

    #[async_trait]
    impl services::Service for TestService {
        async fn on_register(
            &self,
            _endpoint: &hardy_bpv7::eid::Eid,
            _sink: Box<dyn services::ServiceSink>,
        ) {
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            _bundle_id: &hardy_bpv7::bundle::Id,
            _expiry: time::OffsetDateTime,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
        ) -> services::Result<()> {
            Ok(())
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &hardy_bpv7::bundle::Id,
            _from: &hardy_bpv7::eid::Eid,
            _kind: services::StatusNotify,
            _reason: hardy_bpv7::status_report::ReasonCode,
            _timestamp: Option<time::OffsetDateTime>,
        ) {
        }
    }

    // Sends whether the service id is claimed, seen through a free lock,
    // while the factory runs.
    struct ProbeFactory {
        registry: Weak<ServiceRegistry>,
        seen_tx: flume::Sender<Option<bool>>,
    }

    impl services::ServiceFactory for ProbeFactory {
        fn build(self, _cx: ServiceContext) -> services::Result<Arc<dyn services::Service>> {
            let registry = self
                .registry
                .upgrade()
                .expect("the registry outlives the build");
            let _ = self.seen_tx.send(registry.services.is_building(&ID));
            Ok(Arc::new(TestService))
        }
    }

    const ID: hardy_bpv7::eid::Service = hardy_bpv7::eid::Service::Ipn(42);

    // Registers a probe at `ID`, answering the result and what the probe saw.
    async fn register_probe(bpa: &Bpa) -> (services::Result<Eid>, Option<Option<bool>>) {
        let (registry, node_ids, rib, dispatcher) = bpa.service_registry();
        let (seen_tx, seen_rx) = flume::bounded(1);
        let factory = BoxFactory::service(ProbeFactory {
            registry: Arc::downgrade(registry),
            seen_tx,
        });
        let result = registry
            .register(&ID, factory, node_ids, rib, dispatcher)
            .await;
        (result, seen_rx.try_recv().ok())
    }

    // The factory runs against a claim on its service id, outside the
    // registry's lock.
    #[tokio::test]
    async fn the_factory_runs_against_a_claim_outside_the_lock() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;
        let (result, seen) = register_probe(&bpa).await;
        result.expect("registration should succeed");
        assert_eq!(seen, Some(Some(true)), "claimed, through a free lock");
        bpa.shutdown().await;
    }

    // A service id already registered is refused before its factory runs.
    #[tokio::test]
    async fn a_taken_service_id_is_refused_before_the_build() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;
        bpa.register_service(ID, Arc::new(TestService))
            .await
            .unwrap();
        let (result, seen) = register_probe(&bpa).await;
        assert!(
            matches!(result, Err(services::Error::ServiceIdInUse(ref id)) if id == "42"),
            "{result:?}"
        );
        assert_eq!(seen, None, "the factory must never run");
        bpa.shutdown().await;
    }

    // The admin endpoint's service ids cannot be claimed: ipn service 0 and
    // the zero-length dtn demux are rejected at registration instead of
    // being silently shadowed by the admin route's precedence
    // (RFC 9758 Section 5.7).
    #[test]
    fn admin_endpoint_service_ids_are_rejected() {
        let node_ids = node_ids::NodeIds::default();
        let mut builder = ServiceRegistryBuilder::new();
        for admin in [
            hardy_bpv7::eid::Service::Ipn(0),
            hardy_bpv7::eid::Service::Dtn("".into()),
        ] {
            let service = BoxFactory::service(Arc::new(TestService));
            assert!(matches!(
                builder.insert(admin, service, &node_ids),
                Err(services::Error::AdministrativeEndpoint(_))
            ));
        }

        // The neighbouring non-zero id remains registrable.
        let service = BoxFactory::service(Arc::new(TestService));
        builder
            .insert(hardy_bpv7::eid::Service::Ipn(1), service, &node_ids)
            .expect("ipn service 1 is not the admin endpoint");
    }
}
