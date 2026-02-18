use super::*;
use hardy_bpv7::eid::{DtnNodeId, Eid, IpnNodeId};
use rand::{
    RngExt,
    distr::{Alphanumeric, SampleString},
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
    pub service_id: Eid,
}

impl Service {
    pub async fn on_status_notify(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        from: &Eid,
        kind: StatusNotify,
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

/// Sink implementation for both Service and Application traits
struct Sink {
    service: Weak<Service>,
    registry: Arc<Registry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Sink {
    async fn unregister_inner(&self) {
        if let Some(service) = self.service.upgrade() {
            self.registry.unregister(service).await
        }
    }

    async fn cancel_inner(&self, bundle_id: &hardy_bpv7::bundle::Id) -> services::Result<bool> {
        if bundle_id.source
            != self
                .service
                .upgrade()
                .ok_or(services::Error::Disconnected)?
                .service_id
        {
            return Ok(false);
        }

        Ok(self.dispatcher.cancel_local_dispatch(bundle_id).await)
    }
}

#[async_trait]
impl services::ServiceSink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(&self, data: Bytes) -> services::Result<hardy_bpv7::bundle::Id> {
        let service = self
            .service
            .upgrade()
            .ok_or(services::Error::Disconnected)?;

        self.dispatcher
            .local_dispatch_raw(&service.service_id, data)
            .await
    }

    async fn cancel(&self, bundle_id: &hardy_bpv7::bundle::Id) -> services::Result<bool> {
        self.cancel_inner(bundle_id).await
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
        data: Bytes,
        lifetime: core::time::Duration,
        options: Option<services::SendOptions>,
    ) -> services::Result<hardy_bpv7::bundle::Id> {
        let service = self
            .service
            .upgrade()
            .ok_or(services::Error::Disconnected)?;

        self.dispatcher
            .local_dispatch(
                service.service_id.clone(),
                destination,
                data,
                lifetime,
                options,
            )
            .await
    }

    async fn cancel(&self, bundle_id: &hardy_bpv7::bundle::Id) -> services::Result<bool> {
        self.cancel_inner(bundle_id).await
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(service) = self.service.upgrade() {
            let registry = self.registry.clone();
            hardy_async::spawn!(self.registry.tasks, "sink_drop_cleanup", async move {
                registry.unregister(service).await;
            });
        }
    }
}

pub(crate) struct Registry {
    node_ids: node_ids::NodeIds,
    rib: Arc<rib::Rib>,
    // sync::spin::Mutex for O(1) service HashMap operations
    services: hardy_async::sync::spin::Mutex<HashMap<Eid, Arc<Service>>>,
    tasks: hardy_async::TaskPool,
}

impl Registry {
    pub fn new(config: &config::Config, rib: Arc<rib::Rib>) -> Self {
        Self {
            node_ids: config.node_ids.clone(),
            rib,
            services: Default::default(),
            tasks: hardy_async::TaskPool::new(),
        }
    }

    pub async fn shutdown(&self) {
        // sync::spin::Mutex::lock() returns guard directly (no Result)
        let services = self
            .services
            .lock()
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>();

        for service in services {
            self.unregister_service(service).await
        }

        // Wait for all cleanup tasks spawned from Drop handlers
        self.tasks.shutdown().await;
    }

    /// Register an Application (high-level, payload-only access)
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self, app, dispatcher)))]
    pub async fn register_application(
        self: &Arc<Self>,
        service_id: Option<hardy_bpv7::eid::Service>,
        app: Arc<dyn services::Application>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        self.register_inner(service_id, ServiceImpl::Application(app), dispatcher)
            .await
    }

    /// Register a low-level Service directly
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self, service, dispatcher))
    )]
    pub async fn register_service(
        self: &Arc<Self>,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn services::Service>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        self.register_inner(service_id, ServiceImpl::LowLevel(service), dispatcher)
            .await
    }

    /// Internal registration logic shared by both service types
    async fn register_inner(
        self: &Arc<Self>,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: ServiceImpl,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Eid> {
        // Categorize the request: explicit ID vs auto-generated
        enum IdRequest {
            ExplicitIpn {
                fqnn: IpnNodeId,
                service_number: u32,
            },
            ExplicitDtn {
                node_name: DtnNodeId,
                service_name: Box<str>,
            },
            AutoIpn {
                fqnn: IpnNodeId,
            },
            AutoDtn {
                node_name: DtnNodeId,
            },
        }

        // Determine what kind of ID we need (no lock held yet)
        let id_request = if let Some(service_id) = service_id {
            match service_id {
                hardy_bpv7::eid::Service::Dtn(service_name) => {
                    let node_name = self
                        .node_ids
                        .dtn
                        .as_ref()
                        .ok_or(services::Error::NoDtnNodeId)?
                        .clone();

                    if service_name.is_empty() {
                        IdRequest::AutoDtn { node_name }
                    } else {
                        if !DtnNodeId::is_valid_service_name(&service_name) {
                            return Err(services::Error::DtnInvalidServiceName(
                                service_name.to_string(),
                            ));
                        }
                        IdRequest::ExplicitDtn {
                            node_name,
                            service_name,
                        }
                    }
                }
                hardy_bpv7::eid::Service::Ipn(service_number) => {
                    let fqnn = self
                        .node_ids
                        .ipn
                        .as_ref()
                        .ok_or(services::Error::NoIpnNodeId)?
                        .clone();

                    if service_number == 0 {
                        IdRequest::AutoIpn { fqnn }
                    } else {
                        IdRequest::ExplicitIpn {
                            fqnn,
                            service_number,
                        }
                    }
                }
            }
        } else if let Some(fqnn) = &self.node_ids.ipn {
            IdRequest::AutoIpn { fqnn: fqnn.clone() }
        } else if let Some(node_name) = &self.node_ids.dtn {
            IdRequest::AutoDtn {
                node_name: node_name.clone(),
            }
        } else {
            return Err(services::Error::NoIpnNodeId);
        };

        // For auto-generated IDs, we need Option to allow retry loop
        let mut service_impl = Some(service);

        let (service, service_id) = match id_request {
            // Explicit IDs: single attempt, error on collision
            IdRequest::ExplicitIpn {
                fqnn,
                service_number,
            } => {
                let candidate = Eid::Ipn {
                    fqnn,
                    service_number,
                };
                let mut services = self.services.lock();
                if services.contains_key(&candidate) {
                    return Err(services::Error::IpnServiceInUse(service_number));
                }
                let service = Arc::new(Service {
                    service: service_impl.take().unwrap(),
                    service_id: candidate.clone(),
                });
                services.insert(candidate.clone(), service.clone());
                (service, candidate)
            }

            IdRequest::ExplicitDtn {
                node_name,
                service_name,
            } => {
                let candidate = Eid::Dtn {
                    node_name,
                    service_name: service_name.clone(),
                };
                let mut services = self.services.lock();
                if services.contains_key(&candidate) {
                    return Err(services::Error::DtnServiceInUse(service_name.to_string()));
                }
                let service = Arc::new(Service {
                    service: service_impl.take().unwrap(),
                    service_id: candidate.clone(),
                });
                services.insert(candidate.clone(), service.clone());
                (service, candidate)
            }

            // Auto-generated IDs: loop with RNG OUTSIDE lock, check+insert inside
            IdRequest::AutoIpn { fqnn } => {
                loop {
                    // Generate candidate OUTSIDE the lock (RNG call here)
                    let candidate = Eid::Ipn {
                        fqnn: fqnn.clone(),
                        service_number: rand::rng().random_range(0x10000..=u32::MAX),
                    };

                    // Lock scope: O(1) check + insert only
                    let mut services = self.services.lock();
                    if services.contains_key(&candidate) {
                        // Collision - drop lock and try new random value
                        continue;
                    }

                    let service = Arc::new(Service {
                        service: service_impl.take().unwrap(),
                        service_id: candidate.clone(),
                    });
                    services.insert(candidate.clone(), service.clone());
                    break (service, candidate);
                }
            }

            IdRequest::AutoDtn { node_name } => {
                loop {
                    // Generate candidate OUTSIDE the lock (RNG call here)
                    let candidate = Eid::Dtn {
                        node_name: node_name.clone(),
                        service_name: format!(
                            "auto/{}",
                            Alphanumeric.sample_string(&mut rand::rng(), 16)
                        )
                        .into(),
                    };

                    // Lock scope: O(1) check + insert only
                    let mut services = self.services.lock();
                    if services.contains_key(&candidate) {
                        // Collision - drop lock and try new random value
                        continue;
                    }

                    let service = Arc::new(Service {
                        service: service_impl.take().unwrap(),
                        service_id: candidate.clone(),
                    });
                    services.insert(candidate.clone(), service.clone());
                    break (service, candidate);
                }
            }
        };

        info!("Registered new service: {service_id}");

        // Add local service to RIB
        self.rib
            .add_service(service_id.clone(), service.clone())
            .await;

        // Call on_register with appropriate sink type
        let sink = Sink {
            service: Arc::downgrade(&service),
            registry: self.clone(),
            dispatcher: dispatcher.clone(),
        };

        match &service.service {
            ServiceImpl::LowLevel(svc) => {
                svc.on_register(&service_id, Box::new(sink)).await;
            }
            ServiceImpl::Application(app) => {
                app.on_register(&service_id, Box::new(sink)).await;
            }
        }

        Ok(service_id)
    }

    async fn unregister(&self, service: Arc<Service>) {
        // sync::spin::Mutex::lock() returns guard directly (no Result)
        let service = self.services.lock().remove(&service.service_id);

        if let Some(service) = service {
            self.unregister_service(service).await
        }
    }

    async fn unregister_service(&self, service: Arc<Service>) {
        // Remove local service from RIB
        self.rib.remove_service(&service.service_id, &service);

        match &service.service {
            ServiceImpl::LowLevel(svc) => svc.on_unregister().await,
            ServiceImpl::Application(app) => app.on_unregister().await,
        }

        info!("Unregistered service: {}", service.service_id);
    }

    pub async fn find(&self, service_id: &Eid) -> Option<Arc<Service>> {
        // sync::spin::Mutex::lock() returns guard directly (no Result)
        self.services.lock().get(service_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Duplicate Reg' (Attempt to register an active ID)
    // #[test]
    // fn test_duplicate_reg() {
    //     todo!("Verify Attempt to register an active ID");
    // }

    // // TODO: Implement test for 'Cleanup' (Verify ID is freed on disconnect)
    // #[test]
    // fn test_cleanup() {
    //     todo!("Verify ID is freed on disconnect");
    // }
}
