use alloc::vec::Vec;

use futures::FutureExt;
#[cfg(test)]
use hardy_async::async_trait;
use hardy_bpv7::bundle::Id as BundleId;
use hardy_bpv7::eid::{Eid, Service as EidService};
use hardy_bpv7::status_report::ReasonCode;
use tracing::{error, info};

use crate::dispatcher::Dispatcher;
use crate::node_ids::NodeIds;
use crate::rib::Rib;
use crate::services::context::ServiceOp;
use crate::services::{self, Application, Service as ServiceTrait, ServiceContext, StatusNotify};
use crate::{Arc, HashMap};

// ServiceRegistry uses hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. RNG for auto-generated IDs is called OUTSIDE the lock
// 3. Lock only protects contains_key + insert (both O(1))
// 4. No blocking/sleeping while holding lock

/// Distinguishes between low-level Service and high-level Application registrations
pub enum ServiceImpl {
    /// Low-level service with full bundle access
    LowLevel(Arc<dyn ServiceTrait>),
    /// High-level application receiving only payload
    Application(Arc<dyn Application>),
}

pub struct Service {
    pub service: ServiceImpl,
    pub service_id: EidService,
}

impl Service {
    pub async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
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

type ServiceMap = HashMap<EidService, Arc<Service>>;

pub(crate) struct ServiceRegistryBuilder {
    services: ServiceMap,
}

impl ServiceRegistryBuilder {
    pub fn new() -> Self {
        Self {
            services: Default::default(),
        }
    }

    pub fn insert(&mut self, service_id: EidService, service: ServiceImpl) -> services::Result<()> {
        if self.services.contains_key(&service_id) {
            return Err(services::Error::ServiceIdInUse(service_id.to_string()));
        }
        let service = Arc::new(Service {
            service,
            service_id: service_id.clone(),
        });
        self.services.insert(service_id.clone(), service);
        info!("Inserted service: {service_id}");
        Ok(())
    }

    pub async fn build(
        self,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
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
    pub async fn shutdown(&self, node_ids: &NodeIds, rib: &Arc<Rib>) {
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
        service_id: EidService,
        service: Arc<dyn ServiceTrait>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> services::Result<Eid> {
        self.insert_inner(service_id.clone(), ServiceImpl::LowLevel(service))?;
        self.register(&service_id, node_ids, rib, dispatcher).await
    }

    pub async fn register_application(
        self: &Arc<Self>,
        service_id: EidService,
        application: Arc<dyn Application>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> services::Result<Eid> {
        self.insert_inner(service_id.clone(), ServiceImpl::Application(application))?;
        self.register(&service_id, node_ids, rib, dispatcher).await
    }

    /// Register a service with a dynamically assigned IPN service number.
    pub async fn register_dynamic_service(
        self: &Arc<Self>,
        service: Arc<dyn ServiceTrait>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> services::Result<Eid> {
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
    ) -> services::Result<Eid> {
        let service_id = self.allocate_dynamic_id();
        self.register_application(service_id, application, node_ids, rib, dispatcher)
            .await
    }

    fn allocate_dynamic_id(&self) -> EidService {
        let id = self
            .next_dynamic
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        EidService::Ipn(id)
    }

    fn insert_inner(&self, service_id: EidService, service: ServiceImpl) -> services::Result<()> {
        let mut services = self.services.lock();
        if services.contains_key(&service_id) {
            return Err(services::Error::ServiceIdInUse(service_id.to_string()));
        }
        let service = Arc::new(Service {
            service,
            service_id: service_id.clone(),
        });
        services.insert(service_id.clone(), service);
        Ok(())
    }

    async fn register(
        self: &Arc<Self>,
        service_id: &EidService,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
        dispatcher: &Arc<Dispatcher>,
    ) -> services::Result<Eid> {
        let service = self.services.lock().get(service_id).cloned().unwrap();
        let eid = node_ids.resolve_eid(service_id)?;

        rib.add_service(eid.clone(), service.clone()).await;

        let (ops_tx, ops_rx) = flume::unbounded();
        let shutdown = self.tasks.cancel_token().child_token();
        let ctx = ServiceContext::new(ops_tx, eid.clone(), shutdown.clone());

        // Spawn receiver task for service operations
        let registry = self.clone();
        let service_for_task = service.clone();
        let eid_for_task = eid.clone();
        let node_ids_for_task = Arc::new(node_ids.clone());
        let rib_for_task = rib.clone();
        let dispatcher_for_task = dispatcher.clone();
        let cancel = shutdown.clone();
        hardy_async::spawn!(self.tasks, "service_ops_receiver", async move {
            loop {
                futures::select_biased! {
                    _ = cancel.cancelled().fuse() => break,
                    op = ops_rx.recv_async().fuse() => match op {
                        Ok(op) => match op {
                            ServiceOp::SendRaw { data, reply } => {
                                let result = dispatcher_for_task
                                    .local_dispatch_raw(&eid_for_task, data)
                                    .await;
                                let _ = reply.send(result);
                            }
                            ServiceOp::Send {
                                destination,
                                data,
                                lifetime,
                                options,
                                reply,
                            } => {
                                let result = dispatcher_for_task
                                    .local_dispatch(
                                        eid_for_task.clone(),
                                        destination,
                                        data,
                                        lifetime,
                                        options,
                                    )
                                    .await;
                                let _ = reply.send(result);
                            }
                            ServiceOp::Cancel { bundle_id, reply } => {
                                let result = if bundle_id.source != eid_for_task {
                                    Ok(false)
                                } else {
                                    Ok(dispatcher_for_task
                                        .cancel_local_dispatch(&bundle_id)
                                        .await)
                                };
                                let _ = reply.send(result);
                            }
                        },
                        Err(_) => break,
                    },
                }
            }
            // Channel closed or shutdown cancelled: unregister
            if let Err(e) = registry
                .unregister(service_for_task, &node_ids_for_task, &rib_for_task)
                .await
            {
                error!("Failed to unregister service: {e}");
            }
        });

        match &service.service {
            ServiceImpl::LowLevel(s) => s.on_register(&eid, ctx).await,
            ServiceImpl::Application(a) => a.on_register(&eid, ctx).await,
        }
        dispatcher.poll_service_waiting(&eid).await;
        metrics::gauge!("bpa.service.registered").increment(1.0);
        Ok(eid)
    }

    async fn unregister(
        &self,
        service: Arc<Service>,
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
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
        node_ids: &NodeIds,
        rib: &Arc<Rib>,
    ) -> services::Result<()> {
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
        ctx: hardy_async::sync::spin::Once<ServiceContext>,
    }

    impl TestApp {
        fn new() -> Self {
            Self {
                ctx: hardy_async::sync::spin::Once::new(),
            }
        }
    }

    use super::*;

    #[async_trait]
    impl Application for TestApp {
        async fn on_register(&self, _source: &Eid, ctx: ServiceContext) {
            self.ctx.call_once(|| ctx);
        }
        async fn on_unregister(&self) {}
        async fn on_receive(
            &self,
            _source: Eid,
            _expiry: time::OffsetDateTime,
            _ack_requested: bool,
            _payload: bytes::Bytes,
        ) {
        }
        async fn on_status_notify(
            &self,
            _bundle_id: &BundleId,
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
        bpa.start(false);

        let svc_id = EidService::Ipn(42);

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

    // Dropping the ServiceContext triggers channel close, which the receiver
    // task detects and unregisters the service. The service ID is then freed
    // for re-registration on the same BPA instance.
    #[tokio::test]
    async fn test_context_drop_unregisters() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false);

        let svc_id = EidService::Ipn(99);

        // Register with a Mutex-based app so we can take the context
        struct DroppableApp {
            ctx: std::sync::Mutex<Option<ServiceContext>>,
        }
        #[async_trait]
        impl services::Application for DroppableApp {
            async fn on_register(&self, _: &Eid, ctx: ServiceContext) {
                *self.ctx.lock().unwrap() = Some(ctx);
            }
            async fn on_unregister(&self) {}
            async fn on_receive(&self, _: Eid, _: time::OffsetDateTime, _: bool, _: bytes::Bytes) {}
            async fn on_status_notify(
                &self,
                _: &BundleId,
                _: &Eid,
                _: StatusNotify,
                _: ReasonCode,
                _: Option<time::OffsetDateTime>,
            ) {
            }
        }

        let app = Arc::new(DroppableApp {
            ctx: std::sync::Mutex::new(None),
        });
        let result = bpa.register_application(svc_id.clone(), app.clone()).await;
        assert!(result.is_ok());

        // Drop the context to trigger channel-close unregistration
        app.ctx.lock().unwrap().take();

        // Give the receiver task time to detect the close and unregister
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Re-registration with the same service number should succeed
        let app2 = Arc::new(TestApp::new());
        let result = bpa.register_application(svc_id, app2).await;
        assert!(
            result.is_ok(),
            "Re-registration after context drop should succeed, got: {result:?}"
        );

        bpa.shutdown().await;
    }
}
