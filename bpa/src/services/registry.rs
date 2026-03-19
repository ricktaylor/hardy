use bytes::Bytes;
use hardy_async::async_trait;
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::{DtnNodeId, Eid, IpnNodeId, Service as Bpv7Service};
use rand::{
    RngExt,
    distr::{Alphanumeric, SampleString},
};
use tracing::info;

use super::{
    Application, ApplicationSink, Error, Result, SendOptions, Service, ServiceSink, StatusNotify,
};
use crate::dispatcher::Dispatcher;
use crate::node_ids::NodeIds;
use crate::rib::Rib;
use crate::{Arc, HashMap, Weak};

// ServiceRegistry uses hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. RNG for auto-generated IDs is called OUTSIDE the lock
// 3. Lock only protects contains_key + insert (both O(1))
// 4. No blocking/sleeping while holding lock

/// Distinguishes between low-level Service and high-level Application registrations
pub enum ServiceImpl {
    /// Low-level service with full bundle access
    LowLevel(Arc<dyn Service>),
    /// High-level application receiving only payload
    Application(Arc<dyn Application>),
}

pub struct ServiceRecord {
    pub service: ServiceImpl,
    pub service_id: Eid,
}

impl ServiceRecord {
    pub async fn on_status_notify(
        &self,
        bundle_id: &Id,
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
            ServiceImpl::Application(app) => {
                app.on_status_notify(bundle_id, from, kind, reason, timestamp)
                    .await
            }
        }
    }
}

impl PartialEq for ServiceRecord {
    fn eq(&self, other: &Self) -> bool {
        self.service_id == other.service_id
    }
}

impl Eq for ServiceRecord {}

impl PartialOrd for ServiceRecord {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ServiceRecord {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.service_id.cmp(&other.service_id)
    }
}

impl core::hash::Hash for ServiceRecord {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.service_id.hash(state);
    }
}

impl core::fmt::Debug for ServiceRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Service")
            .field("eid", &self.service_id)
            .finish_non_exhaustive()
    }
}

struct Sink {
    service: Weak<ServiceRecord>,
    registry: Arc<ServiceRegistry>,
    dispatcher: Arc<Dispatcher>,
}

impl Sink {
    async fn unregister_inner(&self) {
        if let Some(service) = self.service.upgrade() {
            self.registry.unregister(service).await
        }
    }

    async fn cancel_inner(&self, bundle_id: &Id) -> Result<bool> {
        if bundle_id.source
            != self
                .service
                .upgrade()
                .ok_or(Error::Disconnected)?
                .service_id
        {
            return Ok(false);
        }

        Ok(self.dispatcher.cancel_local_dispatch(bundle_id).await)
    }
}

#[async_trait]
impl ServiceSink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(&self, data: Bytes) -> Result<Id> {
        let service = self.service.upgrade().ok_or(Error::Disconnected)?;

        self.dispatcher
            .local_dispatch_raw(&service.service_id, data)
            .await
    }

    async fn cancel(&self, bundle_id: &Id) -> Result<bool> {
        self.cancel_inner(bundle_id).await
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
        let service = self.service.upgrade().ok_or(Error::Disconnected)?;

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

    async fn cancel(&self, bundle_id: &Id) -> Result<bool> {
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

pub(crate) struct ServiceRegistry {
    node_ids: NodeIds,
    rib: Arc<Rib>,
    // sync::spin::Mutex for O(1) service HashMap operations
    records: hardy_async::sync::spin::Mutex<HashMap<Eid, Arc<ServiceRecord>>>,
    tasks: hardy_async::TaskPool,
}

impl ServiceRegistry {
    pub fn new(node_ids: NodeIds, rib: Arc<Rib>) -> Self {
        Self {
            node_ids,
            rib,
            records: Default::default(),
            tasks: hardy_async::TaskPool::new(),
        }
    }

    pub async fn shutdown(&self) {
        let services = self
            .records
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

    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self, app, dispatcher)))]
    pub async fn register_application(
        self: &Arc<Self>,
        service_id: Option<Bpv7Service>,
        app: Arc<dyn Application>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Eid> {
        self.register_inner(service_id, ServiceImpl::Application(app), dispatcher)
            .await
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self, service, dispatcher))
    )]
    pub async fn register_service(
        self: &Arc<Self>,
        service_id: Option<Bpv7Service>,
        service: Arc<dyn Service>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Eid> {
        self.register_inner(service_id, ServiceImpl::LowLevel(service), dispatcher)
            .await
    }

    async fn register_inner(
        self: &Arc<Self>,
        service_id: Option<Bpv7Service>,
        service: ServiceImpl,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<Eid> {
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

        let id_request = if let Some(service_id) = service_id {
            match service_id {
                Bpv7Service::Dtn(service_name) => {
                    let node_name = self
                        .node_ids
                        .dtn
                        .as_ref()
                        .ok_or(Error::NoDtnNodeId)?
                        .clone();

                    if service_name.is_empty() {
                        IdRequest::AutoDtn { node_name }
                    } else {
                        if !DtnNodeId::is_valid_service_name(&service_name) {
                            return Err(Error::DtnInvalidServiceName(service_name.to_string()));
                        }
                        IdRequest::ExplicitDtn {
                            node_name,
                            service_name,
                        }
                    }
                }
                Bpv7Service::Ipn(service_number) => {
                    let fqnn = self
                        .node_ids
                        .ipn
                        .as_ref()
                        .ok_or(Error::NoIpnNodeId)?
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
            return Err(Error::NoIpnNodeId);
        };

        let mut service_impl = Some(service);

        let (service, service_id) = match id_request {
            IdRequest::ExplicitIpn {
                fqnn,
                service_number,
            } => {
                let candidate = Eid::Ipn {
                    fqnn,
                    service_number,
                };
                let mut records = self.records.lock();
                if records.contains_key(&candidate) {
                    return Err(Error::IpnServiceInUse(service_number));
                }
                let service = Arc::new(ServiceRecord {
                    service: service_impl.take().unwrap(),
                    service_id: candidate.clone(),
                });
                records.insert(candidate.clone(), service.clone());
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
                let mut records = self.records.lock();
                if records.contains_key(&candidate) {
                    return Err(Error::DtnServiceInUse(service_name.to_string()));
                }
                let service = Arc::new(ServiceRecord {
                    service: service_impl.take().unwrap(),
                    service_id: candidate.clone(),
                });
                records.insert(candidate.clone(), service.clone());
                (service, candidate)
            }

            IdRequest::AutoIpn { fqnn } => {
                loop {
                    let candidate = Eid::Ipn {
                        fqnn: fqnn.clone(),
                        service_number: rand::rng().random_range(0x10000..=u32::MAX),
                    };

                    let mut records = self.records.lock();
                    if records.contains_key(&candidate) {
                        // Collision - drop lock and try new random value
                        continue;
                    }

                    let service = Arc::new(ServiceRecord {
                        service: service_impl.take().unwrap(),
                        service_id: candidate.clone(),
                    });
                    records.insert(candidate.clone(), service.clone());
                    break (service, candidate);
                }
            }

            IdRequest::AutoDtn { node_name } => {
                loop {
                    let candidate = Eid::Dtn {
                        node_name: node_name.clone(),
                        service_name: format!(
                            "auto/{}",
                            Alphanumeric.sample_string(&mut rand::rng(), 16)
                        )
                        .into(),
                    };

                    let mut records = self.records.lock();
                    if records.contains_key(&candidate) {
                        // collision - drop lock and try new random value
                        continue;
                    }

                    let service = Arc::new(ServiceRecord {
                        service: service_impl.take().unwrap(),
                        service_id: candidate.clone(),
                    });
                    records.insert(candidate.clone(), service.clone());
                    break (service, candidate);
                }
            }
        };

        info!("Registered new service: {service_id}");

        self.rib
            .add_service(service_id.clone(), service.clone())
            .await;

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
        dispatcher.poll_service_waiting(&service_id).await;

        Ok(service_id)
    }

    async fn unregister(&self, service: Arc<ServiceRecord>) {
        let service = self.records.lock().remove(&service.service_id);

        if let Some(service) = service {
            self.unregister_service(service).await
        }
    }

    async fn unregister_service(&self, service: Arc<ServiceRecord>) {
        self.rib.remove_service(&service.service_id, &service);

        match &service.service {
            ServiceImpl::LowLevel(svc) => svc.on_unregister().await,
            ServiceImpl::Application(app) => app.on_unregister().await,
        }

        info!("Unregistered service: {}", service.service_id);
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
