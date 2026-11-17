use hardy_async::{CancellationToken, TaskPool, async_trait, sync::spin::Mutex};
// `eid::Service` is the service number inside an EID, not the trait of the
// same name that this module registers; it is aliased to the word the code
// already uses for it.
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, Service as ServiceId},
    status_report::ReasonCode,
};
use portable_atomic::{AtomicU32, Ordering};
use tracing::{error, info};

use super::{
    Application, ApplicationSink, Error, Result, SendOptions, Service, ServiceSink, StatusNotify,
};
use crate::{
    Arc, Bytes, HashMap, Weak,
    bundle::Bundle,
    dispatcher::Dispatcher,
    node_ids::{NodeIds, service_is_admin_endpoint},
    routing::Rib,
    storage::channel::{SendError, Sender},
};

// The service map is behind a spin lock. Its critical section is a
// `contains_key` and an insert, both O(1), with id allocation done outside
// it, so the lock is never held long enough to repay an OS mutex's park.

/// Distinguishes between low-level Service and high-level Application registrations
pub enum ServiceImpl {
    /// Low-level service with full bundle access
    LowLevel(Arc<dyn Service>),
    /// High-level application receiving only payload
    Application(Arc<dyn Application>),
}

pub struct Record {
    pub service: ServiceImpl,
    pub service_id: ServiceId,
    // Cancelled at unregistration; every in-flight stream of this
    // registration races it and fails immediately.
    pub cancel: CancellationToken,
    // The canonical registration EID — the exact key the DeliverPending
    // claim, the unregister sweep, and poll_service_waiting match on.
    eid: Eid,
    // This registration's delivery queue (the local analogue of a peer's
    // egress queues).
    tx: Sender,
}

impl Record {
    /// The sole constructor: a reachable `Record` always carries its live
    /// delivery queue and its canonical registration EID, so no consumer
    /// can observe a half-built registration.
    pub fn new(service: ServiceImpl, service_id: ServiceId, eid: Eid, tx: Sender) -> Arc<Self> {
        Arc::new(Self {
            service,
            service_id,
            cancel: CancellationToken::new(),
            eid,
            tx,
        })
    }

    /// Queue a bundle into this service's delivery channel.
    ///
    /// `Err(bundle)` hands ownership back for parking (the mirror of
    /// `Peer::forward`): the queue has closed (unregistration).
    #[expect(
        clippy::result_large_err,
        reason = "Err(bundle) hands ownership back to the caller; boxing it to shrink the Err variant would allocate on every call"
    )]
    pub async fn deliver(&self, bundle: Bundle) -> core::result::Result<(), Bundle> {
        self.tx.send(bundle).await.map_err(|SendError(b)| b)
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
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
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

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.service_id == other.service_id
    }
}

impl Eq for Record {}

impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Record {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.service_id.cmp(&other.service_id)
    }
}

impl core::hash::Hash for Record {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.service_id.hash(state);
    }
}

impl core::fmt::Debug for Record {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Record")
            .field("eid", &self.service_id)
            .finish_non_exhaustive()
    }
}

/// Sink implementation for both the Service and Application traits
// The back-edges are weak. `Bpa` owns the registry, the RIB and the
// dispatcher for the life of the BPA, so a strong edge would add no
// reachability — only a cycle through the service that holds this sink,
// and a cycle of `Arc`s never drops. A failed upgrade is the condition the
// trait already names, `Error::Disconnected`.
struct Sink {
    service: Weak<Record>,
    /// Full EID for this service (pre-resolved at activation time)
    eid: Eid,
    registry: Weak<ServiceRegistry>,
    rib: Weak<Rib>,
    dispatcher: Weak<Dispatcher>,
}

impl Sink {
    async fn unregister_inner(&self) {
        if let (Some(service), Some(registry), Some(rib)) = (
            self.service.upgrade(),
            self.registry.upgrade(),
            self.rib.upgrade(),
        ) && let Err(e) = registry.unregister(service, &rib).await
        {
            error!("Failed to unregister service: {e}");
        }
    }
}

#[async_trait]
impl ServiceSink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(
        &self,
        stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
    ) -> Result<Id> {
        let service = self.service.upgrade().ok_or(Error::Disconnected)?;
        let dispatcher = self.dispatcher.upgrade().ok_or(Error::Disconnected)?;

        // A service that unregisters mid-send must not originate its bundle:
        // the registration's token races every pull, so teardown wakes this
        // stream immediately — even parked behind a stalled producer — and
        // the send surfaces as cancelled. Sink-side, so the dispatcher's
        // stream consumers stay registration-agnostic.
        let mut stream = crate::stream::CancellableReceiver {
            inner: stream,
            token: service.cancel.clone(),
        };
        dispatcher.originate_raw(&self.eid, &mut stream).await
    }
}

#[async_trait]
impl ApplicationSink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: core::time::Duration,
        options: Option<SendOptions>,
    ) -> Result<Id> {
        self.service.upgrade().ok_or(Error::Disconnected)?;
        let dispatcher = self.dispatcher.upgrade().ok_or(Error::Disconnected)?;

        dispatcher
            .originate(self.eid.clone(), destination, data, lifetime, options)
            .await
    }

    async fn send_streamed(
        &self,
        destination: Eid,
        total_len: u64,
        stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
        lifetime: core::time::Duration,
        options: Option<SendOptions>,
    ) -> Result<Id> {
        let service = self.service.upgrade().ok_or(Error::Disconnected)?;
        let dispatcher = self.dispatcher.upgrade().ok_or(Error::Disconnected)?;

        // Registration teardown wakes an in-flight send exactly as on the
        // raw door: the token races every pull, sink-side.
        let mut stream = crate::stream::CancellableReceiver {
            inner: stream,
            token: service.cancel.clone(),
        };
        dispatcher
            .originate_streamed(
                self.eid.clone(),
                destination,
                total_len,
                &mut stream,
                lifetime,
                options,
            )
            .await
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let (Some(service), Some(registry), Some(rib)) = (
            self.service.upgrade(),
            self.registry.upgrade(),
            self.rib.upgrade(),
        ) {
            let tasks = registry.tasks.clone();
            hardy_async::spawn!(tasks, "sink_drop_cleanup", async move {
                if let Err(e) = registry.unregister(service, &rib).await {
                    error!("Failed to unregister service: {e}");
                }
            });
        }
    }
}

type ServiceMap = HashMap<ServiceId, Arc<Record>>;

pub struct ServiceRegistryBuilder {
    services: Vec<(ServiceId, ServiceImpl)>,
}

impl ServiceRegistryBuilder {
    pub fn new() -> Self {
        Self {
            services: Default::default(),
        }
    }

    pub fn insert(&mut self, service_id: ServiceId, service: ServiceImpl) -> Result<()> {
        if service_is_admin_endpoint(&service_id) {
            return Err(Error::AdministrativeEndpoint(service_id.to_string()));
        }
        if self.services.iter().any(|(id, _)| *id == service_id) {
            return Err(Error::ServiceIdInUse(service_id.to_string()));
        }
        info!("Inserted service: {service_id}");
        self.services.push((service_id, service));
        Ok(())
    }

    /// Construct the registry with the configured registrations parked.
    /// Activation happens in [`ServiceRegistry::start`], after storage
    /// recovery — registrations must not race the consistency check — so
    /// configuration errors are surfaced here, where the build can fail.
    pub fn build(self, node_ids: &NodeIds) -> Result<Arc<ServiceRegistry>> {
        for (service_id, _) in &self.services {
            node_ids.resolve_eid(service_id)?;
        }
        Ok(Arc::new(ServiceRegistry {
            services: Mutex::new(Default::default()),
            pending: Mutex::new(self.services),
            next_dynamic: AtomicU32::new(DYNAMIC_SERVICE_BASE),
            tasks: TaskPool::new(),
        }))
    }
}

/// Base for dynamically assigned IPN service numbers.
/// Starts high to avoid collisions with explicitly assigned IDs.
const DYNAMIC_SERVICE_BASE: u32 = 0x8000_0000;

pub struct ServiceRegistry {
    services: Mutex<ServiceMap>,
    // Builder-configured registrations, parked until `start` activates them
    // after storage recovery.
    pending: Mutex<Vec<(ServiceId, ServiceImpl)>>,
    next_dynamic: AtomicU32,
    tasks: TaskPool,
}

impl ServiceRegistry {
    /// Activate the builder-configured registrations, in configuration
    /// order. Called by `Bpa::start` once storage recovery has completed:
    /// a registration going live earlier would race the consistency check.
    pub async fn start(
        self: &Arc<Self>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) {
        let pending = core::mem::take(&mut *self.pending.lock());
        for (service_id, service) in pending {
            // Validated at build; a failure here is a bug, not a config error.
            if let Err(e) = self
                .register(&service_id, service, node_ids, rib, dispatcher)
                .await
            {
                error!("Failed to activate configured service {service_id}: {e}");
            }
        }
    }
    pub async fn shutdown(&self, rib: &Arc<Rib>) {
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
        service_id: ServiceId,
        service: Arc<dyn Service>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Eid> {
        self.register(
            &service_id,
            ServiceImpl::LowLevel(service),
            node_ids,
            rib,
            dispatcher,
        )
        .await
    }

    pub async fn register_application(
        self: &Arc<Self>,
        service_id: ServiceId,
        application: Arc<dyn Application>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Eid> {
        self.register(
            &service_id,
            ServiceImpl::Application(application),
            node_ids,
            rib,
            dispatcher,
        )
        .await
    }

    /// Register a service with a dynamically assigned IPN service number.
    pub async fn register_dynamic_service(
        self: &Arc<Self>,
        service: Arc<dyn Service>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Eid> {
        let service_id = self.allocate_dynamic_id();
        self.register_service(service_id, service, node_ids, rib, dispatcher)
            .await
    }

    /// Register an application with a dynamically assigned IPN service number.
    pub async fn register_dynamic_application(
        self: &Arc<Self>,
        application: Arc<dyn Application>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Eid> {
        let service_id = self.allocate_dynamic_id();
        self.register_application(service_id, application, node_ids, rib, dispatcher)
            .await
    }

    fn allocate_dynamic_id(&self) -> ServiceId {
        let id = self.next_dynamic.fetch_add(1, Ordering::Relaxed);
        ServiceId::Ipn(id)
    }

    async fn register(
        self: &Arc<Self>,
        service_id: &ServiceId,
        service: ServiceImpl,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Eid> {
        if service_is_admin_endpoint(service_id) {
            return Err(Error::AdministrativeEndpoint(service_id.to_string()));
        }
        let eid = node_ids.resolve_eid(service_id)?;

        // Construct the service complete — the delivery queue and canonical
        // EID are constructor arguments — then publish atomically with the
        // duplicate check: nothing reachable is ever half-built, and the
        // RIB (added below) only ever sees a deliverable service. The
        // channel's creation-time poll recovers any DeliverPending bundles
        // left over from a previous registration under the same EID.
        let (tx, rx) = dispatcher.new_delivery_channel(&eid);
        let service = Record::new(service, service_id.clone(), eid.clone(), tx);
        {
            let mut services = self.services.lock();
            if services.contains_key(service_id) {
                // The loser closes the channel it opened: the channel
                // spawns its storage poller at creation, and dropping the
                // sender does not close it — an unclosed loser parks that
                // poller until store shutdown.
                service.close_queue();
                return Err(Error::ServiceIdInUse(service_id.to_string()));
            }
            services.insert(service_id.clone(), service.clone());
        }
        dispatcher.start_delivery_queue(service.clone(), rx);

        let _ = rib.add_service(eid.clone(), service.clone()).await;

        let sink = Sink {
            service: Arc::downgrade(&service),
            eid: eid.clone(),
            registry: Arc::downgrade(self),
            rib: Arc::downgrade(rib),
            dispatcher: Arc::downgrade(dispatcher),
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
            let eid = eid.clone();
            hardy_async::spawn!(self.tasks, "post_registration_poll", async move {
                dispatcher.poll_service_waiting(&eid).await;
            });
        }
        metrics::gauge!("bpa.service.registered").increment(1.0);
        Ok(eid)
    }

    async fn unregister(&self, service: Arc<Record>, rib: &Arc<Rib>) -> Result<()> {
        let service = self.services.lock().remove(&service.service_id);

        if let Some(service) = service {
            metrics::gauge!("bpa.service.registered").decrement(1.0);
            self.unregister_service(service, rib).await?;
        }
        Ok(())
    }

    async fn unregister_service(&self, service: Arc<Record>, rib: &Arc<Rib>) -> Result<()> {
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
    use hardy_async::sync::spin::Once;

    use super::*;
    use crate::bpa::{Bpa, BpaRegistration};

    struct TestApp {
        sink: Once<Box<dyn ApplicationSink>>,
    }

    impl TestApp {
        fn new() -> Self {
            Self { sink: Once::new() }
        }
    }

    #[async_trait]
    impl Application for TestApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
            self.sink.call_once(|| sink);
        }
        async fn on_unregister(&self) {}
        async fn on_deliver(
            &self,
            _bundle_id: &Id,
            _expiry: time::OffsetDateTime,
            _ack_requested: bool,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
        ) -> Result<()> {
            Ok(())
        }
        async fn on_status_notify(
            &self,
            _bundle_id: &Id,
            _from: &Eid,
            _kind: StatusNotify,
            _reason: ReasonCode,
            _timestamp: Option<time::OffsetDateTime>,
        ) {
        }
    }

    // Registering two applications with the same explicit IPN service number should fail.
    #[tokio::test]
    async fn test_duplicate_reg() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;

        let svc_id = ServiceId::Ipn(42);

        let app1 = Arc::new(TestApp::new());
        let result = bpa.register_application(svc_id.clone(), app1).await;
        assert!(result.is_ok(), "First registration should succeed");

        let app2 = Arc::new(TestApp::new());
        let result = bpa.register_application(svc_id, app2).await;
        assert!(
            matches!(result, Err(Error::ServiceIdInUse(ref id)) if id == "42"),
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

        let svc_id = ServiceId::Ipn(99);

        let app1 = Arc::new(TestApp::new());
        let result = bpa.register_application(svc_id.clone(), app1.clone()).await;
        assert!(result.is_ok());

        app1.sink
            .get()
            .expect("Sink should be set")
            .unregister()
            .await;

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
    impl Service for TestService {
        async fn on_register(&self, _endpoint: &Eid, _sink: Box<dyn ServiceSink>) {}

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            _bundle_id: &Id,
            _expiry: time::OffsetDateTime,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
        ) -> Result<()> {
            Ok(())
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &Id,
            _from: &Eid,
            _kind: StatusNotify,
            _reason: ReasonCode,
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
        let mut builder = ServiceRegistryBuilder::new();
        assert!(matches!(
            builder.insert(
                ServiceId::Ipn(0),
                ServiceImpl::LowLevel(Arc::new(TestService)),
            ),
            Err(Error::AdministrativeEndpoint(_))
        ));
        assert!(matches!(
            builder.insert(
                ServiceId::Dtn("".into()),
                ServiceImpl::LowLevel(Arc::new(TestService)),
            ),
            Err(Error::AdministrativeEndpoint(_))
        ));

        // The neighbouring non-zero / non-empty ids remain registrable.
        assert!(
            builder
                .insert(
                    ServiceId::Ipn(1),
                    ServiceImpl::LowLevel(Arc::new(TestService)),
                )
                .is_ok()
        );
    }
}
