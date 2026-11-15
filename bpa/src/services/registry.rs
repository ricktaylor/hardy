use alloc::vec::Vec;

use hardy_async::closeable;
use hardy_bpv7::eid::Eid;
use portable_atomic::{AtomicU32, Ordering};
use trace_err::*;
use tracing::{error, info};

use super::sink::Sink;
use crate::{Arc, HashMap, bundle, dispatcher, node_ids, routing, services, storage};

// ServiceRegistry uses hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. RNG for auto-generated IDs is called OUTSIDE the lock
// 3. Lock only protects contains_key + insert (both O(1))
// 4. No blocking/sleeping while holding lock

/// Distinguishes between low-level Service and high-level Application registrations
#[derive(Clone)]
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
    pub cancel: hardy_async::CancellationToken,
    // The canonical registration EID — the exact key the DeliverPending
    // claim, the unregister sweep, and poll_service_waiting match on.
    eid: Eid,
    // This registration's delivery queue (the local analogue of a peer's
    // egress queues).
    tx: storage::channel::Sender,
}

impl Service {
    /// The sole constructor: a reachable `Service` always carries its live
    /// delivery queue and its canonical registration EID, so no consumer
    /// can observe a half-built registration.
    pub fn new(
        service: ServiceImpl,
        service_id: hardy_bpv7::eid::Service,
        eid: Eid,
        tx: storage::channel::Sender,
    ) -> Arc<Self> {
        Arc::new(Self {
            service,
            service_id,
            cancel: hardy_async::CancellationToken::new(),
            eid,
            tx,
        })
    }

    /// Queue a bundle into this service's delivery channel.
    ///
    /// `Err(bundle)` hands ownership back for parking (the mirror of
    /// `Peer::forward`): the queue has closed (unregistration).
    // Err(bundle) deliberately hands ownership back to the caller; boxing
    // the bundle to shrink the Err variant would tax every call site.
    #[allow(clippy::result_large_err)]
    pub async fn deliver(
        &self,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        self.tx
            .send(bundle)
            .await
            .map_err(|storage::channel::SendError(b)| b)
    }

    /// The canonical registration EID.
    pub fn eid(&self) -> &Eid {
        &self.eid
    }

    fn close_queue(&self) {
        self.tx.close();
    }
    pub async fn on_status_notify(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        from: &Eid,
        kind: services::StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    ) {
        match &self.service {
            ServiceImpl::LowLevel(svc) => {
                svc.on_status_notify(bundle_id, from, kind, reason, timestamp)
                    .await
            }
            services::registry::ServiceImpl::Application(app) => {
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

type ServiceMap = HashMap<hardy_bpv7::eid::Service, Arc<Service>>;

// One configured registration: a service or application and the ids it
// registers on, all or none.
type PendingRegistration = (Vec<hardy_bpv7::eid::Service>, ServiceImpl);

pub struct ServiceRegistryBuilder {
    services: Vec<PendingRegistration>,
}

impl ServiceRegistryBuilder {
    pub fn new() -> Self {
        Self {
            services: Default::default(),
        }
    }

    // Parks a configured registration on `service_ids`, refusing every id
    // that activation would: the administrative endpoint, one whose scheme
    // has no node id, and one taken by this or an earlier registration.
    // Activation happens in [`ServiceRegistry::start`], after storage
    // recovery — registrations must not race the consistency check — so
    // configuration errors are surfaced here, where the build can fail.
    pub fn insert(
        &mut self,
        service_ids: Vec<hardy_bpv7::eid::Service>,
        service: ServiceImpl,
        node_ids: &node_ids::NodeIds,
    ) -> services::Result<()> {
        resolve_endpoints(&service_ids, node_ids)?;
        if let Some(taken) = service_ids.iter().find(|service_id| {
            self.services
                .iter()
                .any(|(ids, _)| ids.contains(service_id))
        }) {
            return Err(services::Error::ServiceIdInUse(taken.to_string()));
        }
        for service_id in &service_ids {
            info!("Inserted service: {service_id}");
        }
        self.services.push((service_ids, service));
        Ok(())
    }

    // Constructs the registry with the configured registrations parked.
    pub fn build(self) -> Arc<ServiceRegistry> {
        Arc::new(ServiceRegistry {
            services: hardy_async::sync::spin::Mutex::new(Default::default()),
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
    services: hardy_async::sync::spin::Mutex<ServiceMap>,
    // Builder-configured registrations, parked until `start` activates them
    // after storage recovery.
    pending: hardy_async::sync::spin::Mutex<Vec<PendingRegistration>>,
    next_dynamic: AtomicU32,
    // Shared with `Sink`, whose drop cleanup is spawned here.
    pub(super) tasks: hardy_async::TaskPool,
}

impl ServiceRegistry {
    /// Activate the builder-configured registrations, in configuration
    /// order. Called by `Bpa::start` once storage recovery has completed:
    /// a registration going live earlier would race the consistency check.
    pub async fn start(
        self: &Arc<Self>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) {
        let pending = core::mem::take(&mut *self.pending.lock());
        for (service_ids, service) in pending {
            // Validated at build; a failure here is a bug, not a config error.
            if let Err(e) = self
                .register_endpoints(&service_ids, service, node_ids, rib, dispatcher)
                .await
            {
                error!("Failed to activate configured service {service_ids:?}: {e}");
            }
        }
    }
    pub async fn shutdown(&self, rib: &Arc<routing::Rib>) {
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
        self.register_endpoints(
            &[service_id],
            ServiceImpl::LowLevel(service),
            node_ids,
            rib,
            dispatcher,
        )
        .await
        .map(single_eid)
    }

    pub async fn register_application(
        self: &Arc<Self>,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn services::Application>,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        self.register_endpoints(
            &[service_id],
            ServiceImpl::Application(application),
            node_ids,
            rib,
            dispatcher,
        )
        .await
        .map(single_eid)
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

    // Registers one service or application on every id in `service_ids`,
    // all or none: every id is validated, and the set is published under
    // one lock, before any registration goes live. Each id then becomes its
    // own registration, with its own delivery queue and sink. Returns the
    // EIDs in `service_ids` order.
    pub async fn register_endpoints(
        self: &Arc<Self>,
        service_ids: &[hardy_bpv7::eid::Service],
        service: ServiceImpl,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Vec<Eid>> {
        let eids = resolve_endpoints(service_ids, node_ids)?;

        // Construct every registration complete — the delivery queue and
        // canonical EID are constructor arguments — then publish them all
        // atomically with the duplicate check: nothing reachable is ever
        // half-built, the RIB (added below) only ever sees a deliverable
        // service, and a taken id publishes none of the set. Each channel's
        // creation-time poll recovers any DeliverPending bundles left over
        // from a previous registration under the same EID.
        let registrations: Vec<_> = service_ids
            .iter()
            .zip(&eids)
            .map(|(service_id, eid)| {
                let (tx, rx) = dispatcher.new_delivery_channel(eid);
                let registration =
                    Service::new(service.clone(), service_id.clone(), eid.clone(), tx);
                (registration, rx)
            })
            .collect();
        if let Err(e) = self.publish(&registrations) {
            // The losers close the channels they opened: each channel
            // spawns its storage poller at creation, and dropping the
            // sender does not close it — an unclosed loser parks that
            // poller until store shutdown.
            for (registration, _) in &registrations {
                registration.close_queue();
            }
            return Err(e);
        }

        for (registration, rx) in registrations {
            self.activate(registration, rx, rib, dispatcher).await;
        }
        Ok(eids)
    }

    // Publishes every registration, or none if any service id is taken.
    fn publish(
        &self,
        registrations: &[(Arc<Service>, closeable::Receiver<bundle::Bundle>)],
    ) -> services::Result<()> {
        let mut services = self.services.lock();
        if let Some((taken, _)) = registrations
            .iter()
            .find(|(registration, _)| services.contains_key(&registration.service_id))
        {
            return Err(services::Error::ServiceIdInUse(
                taken.service_id.to_string(),
            ));
        }
        for (registration, _) in registrations {
            services.insert(registration.service_id.clone(), registration.clone());
        }
        Ok(())
    }

    // Brings one published registration live: its delivery queue, its
    // route, then `on_register` with its own sink.
    async fn activate(
        self: &Arc<Self>,
        service: Arc<Service>,
        rx: closeable::Receiver<bundle::Bundle>,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) {
        let eid = service.eid().clone();
        dispatcher.start_delivery_queue(service.clone(), rx);

        let _ = rib.add_service(eid.clone(), service.clone()).await;

        let sink = Sink {
            service: Arc::downgrade(&service),
            eid: eid.clone(),
            registry: self.clone(),
            rib: rib.clone(),
            dispatcher: dispatcher.clone(),
        };
        match &service.service {
            ServiceImpl::LowLevel(s) => s.on_register(&eid, Box::new(sink)).await,
            ServiceImpl::Application(a) => a.on_register(&eid, Box::new(sink)).await,
        }
        // The post-registration poll is spawned, never awaited inline: a
        // sink whose event buffer drains only after registration returns —
        // the gRPC-bridge shape — would deadlock registration against its
        // own announcements. Overlapping polls are safe: the poll claims
        // each bundle out of WaitingForService with a status CAS.
        {
            let dispatcher = dispatcher.clone();
            hardy_async::spawn!(self.tasks, "post_registration_poll", async move {
                dispatcher.poll_service_waiting(&eid).await;
            });
        }
        metrics::gauge!("bpa.service.registered").increment(1.0);
        info!("Registered service: {}", service.eid());
    }

    pub(super) async fn unregister(
        &self,
        service: Arc<Service>,
        rib: &Arc<routing::Rib>,
    ) -> services::Result<()> {
        let service = self.services.lock().remove(&service.service_id);

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
            ServiceImpl::LowLevel(svc) => svc.on_unregister(&eid).await,
            ServiceImpl::Application(app) => app.on_unregister(&eid).await,
        }

        info!("Unregistered service: {eid}");
        Ok(())
    }
}

// Resolves each id of a multi-endpoint registration to its EID, refusing
// the set if any id is the administrative endpoint, has no node id for its
// scheme, or appears twice.
fn resolve_endpoints(
    service_ids: &[hardy_bpv7::eid::Service],
    node_ids: &node_ids::NodeIds,
) -> services::Result<Vec<Eid>> {
    if service_ids.is_empty() {
        return Err(services::Error::NoServiceIds);
    }
    let mut eids = Vec::with_capacity(service_ids.len());
    for (i, service_id) in service_ids.iter().enumerate() {
        if node_ids::service_is_admin_endpoint(service_id) {
            return Err(services::Error::AdministrativeEndpoint(
                service_id.to_string(),
            ));
        }
        if service_ids[..i].contains(service_id) {
            return Err(services::Error::ServiceIdInUse(service_id.to_string()));
        }
        eids.push(node_ids.resolve_eid(service_id)?);
    }
    Ok(eids)
}

// The one EID a single-id registration resolved to.
fn single_eid(mut eids: Vec<Eid>) -> Eid {
    eids.pop()
        .trace_expect("a single-id registration resolves exactly one EID")
}

#[cfg(test)]
mod tests {
    use hardy_async::async_trait;

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
        async fn on_unregister(&self, _endpoint: &Eid) {}
        async fn on_deliver(
            &self,
            _endpoint: &Eid,
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

        async fn on_unregister(&self, _endpoint: &Eid) {}

        async fn on_deliver(
            &self,
            _endpoint: &Eid,
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
            let service = ServiceImpl::LowLevel(Arc::new(TestService));
            assert!(matches!(
                builder.insert(vec![admin], service, &node_ids),
                Err(services::Error::AdministrativeEndpoint(_))
            ));
        }

        // The neighbouring non-zero id remains registrable.
        let service = ServiceImpl::LowLevel(Arc::new(TestService));
        builder
            .insert(vec![hardy_bpv7::eid::Service::Ipn(1)], service, &node_ids)
            .expect("ipn service 1 is not the admin endpoint");
    }
}
