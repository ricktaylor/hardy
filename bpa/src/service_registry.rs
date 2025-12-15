use super::*;
use hardy_bpv7::eid::{DtnNodeId, Eid, IpnNodeId};
use rand::{
    Rng,
    distr::{Alphanumeric, SampleString},
};
use std::{
    collections::HashMap,
    sync::{RwLock, Weak},
};

pub struct Service {
    pub service: Arc<dyn service::Service>,
    pub service_id: Eid,
}

impl PartialEq for Service {
    fn eq(&self, other: &Self) -> bool {
        self.service_id == other.service_id
    }
}

impl Eq for Service {}

impl PartialOrd for Service {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Service {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.service_id.cmp(&other.service_id)
    }
}

impl std::hash::Hash for Service {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.service_id.hash(state);
    }
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("eid", &self.service_id)
            .finish()
    }
}

struct Sink {
    service: Weak<Service>,
    registry: Arc<ServiceRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

#[async_trait]
impl service::Sink for Sink {
    async fn unregister(&self) {
        if let Some(service) = self.service.upgrade() {
            self.registry.unregister(service).await
        }
    }

    async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: std::time::Duration,
        options: Option<service::SendOptions>,
    ) -> service::Result<Box<str>> {
        // Sanity check
        if destination.is_null() {
            return Err(service::Error::InvalidDestination(destination));
        }

        Ok(self
            .dispatcher
            .local_dispatch(
                self.service
                    .upgrade()
                    .ok_or(service::Error::Disconnected)?
                    .service_id
                    .clone(),
                destination,
                data,
                lifetime,
                options,
            )
            .await?
            .to_key()
            .into())
    }

    async fn cancel(&self, bundle_id: &str) -> service::Result<bool> {
        let Ok(bundle_id) = hardy_bpv7::bundle::Id::from_key(bundle_id) else {
            return Ok(false);
        };

        if bundle_id.source
            != self
                .service
                .upgrade()
                .ok_or(service::Error::Disconnected)?
                .service_id
        {
            return Ok(false);
        }

        Ok(self.dispatcher.cancel_local_dispatch(&bundle_id).await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(service) = self.service.upgrade() {
            tokio::runtime::Handle::current().block_on(self.registry.unregister(service));
        }
    }
}

pub struct ServiceRegistry {
    node_ids: node_ids::NodeIds,
    rib: Arc<rib::Rib>,
    services: RwLock<HashMap<Eid, Arc<Service>>>,
}

impl ServiceRegistry {
    pub fn new(config: &config::Config, rib: Arc<rib::Rib>) -> Self {
        Self {
            node_ids: config.node_ids.clone(),
            rib,
            services: Default::default(),
        }
    }

    pub async fn shutdown(&self) {
        let services = self
            .services
            .write()
            .trace_expect("Failed to lock mutex")
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>();

        for service in services {
            self.unregister_service(service).await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, service, dispatcher)))]
    pub async fn register(
        self: &Arc<Self>,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn service::Service>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> service::Result<Eid> {
        // Scope the lock
        let (service, service_id) = {
            let mut services = self.services.write().trace_expect("Failed to lock mutex");

            let new_ipn_service = |fqnn: &IpnNodeId| {
                let mut rng = rand::rng();
                loop {
                    let service_id = Eid::Ipn {
                        fqnn: fqnn.clone(),
                        service_number: rng.random_range(0x10000..=u32::MAX),
                    };
                    if !services.contains_key(&service_id) {
                        break service_id;
                    }
                }
            };
            let new_dtn_service = |node_name: &DtnNodeId| {
                let mut rng = rand::rng();
                loop {
                    let service_id = Eid::Dtn {
                        node_name: node_name.clone(),
                        service_name: format!("auto/{}", Alphanumeric.sample_string(&mut rng, 16))
                            .into(),
                    };
                    if !services.contains_key(&service_id) {
                        break service_id;
                    }
                }
            };

            // Compose service EID
            let service_id = if let Some(service_id) = service_id {
                match &service_id {
                    hardy_bpv7::eid::Service::Dtn(service_name) => {
                        let node_name = self
                            .node_ids
                            .dtn
                            .as_ref()
                            .ok_or(service::Error::NoDtnNodeId)?;

                        if service_name.is_empty() {
                            new_dtn_service(node_name)
                        } else {
                            if !DtnNodeId::is_valid_service_name(service_name) {
                                return Err(service::Error::DtnInvalidServiceName(
                                    service_name.to_string(),
                                ));
                            }

                            let service_id = Eid::Dtn {
                                node_name: node_name.clone(),
                                service_name: service_name.clone(),
                            };

                            if services.contains_key(&service_id) {
                                return Err(service::Error::DtnServiceInUse(
                                    service_name.to_string(),
                                ));
                            }
                            service_id
                        }
                    }
                    hardy_bpv7::eid::Service::Ipn(service_number) => {
                        let fqnn = self
                            .node_ids
                            .ipn
                            .as_ref()
                            .ok_or(service::Error::NoIpnNodeId)?;

                        if service_number == &0 {
                            new_ipn_service(fqnn)
                        } else {
                            let service_id = Eid::Ipn {
                                fqnn: fqnn.clone(),
                                service_number: *service_number,
                            };
                            if services.contains_key(&service_id) {
                                return Err(service::Error::IpnServiceInUse(*service_number));
                            }
                            service_id
                        }
                    }
                }
            } else if let Some(fqnn) = &self.node_ids.ipn {
                new_ipn_service(fqnn)
            } else if let Some(node_name) = &self.node_ids.dtn {
                new_dtn_service(node_name)
            } else {
                return Err(service::Error::NoIpnNodeId);
            };

            let service = Arc::new(Service {
                service,
                service_id: service_id.clone(),
            });
            services.insert(service_id.clone(), service.clone());
            (service, service_id)
        };

        info!("Registered new service: {service_id}");

        service
            .service
            .on_register(
                &service_id,
                Box::new(Sink {
                    service: Arc::downgrade(&service),
                    registry: self.clone(),
                    dispatcher: dispatcher.clone(),
                }),
            )
            .await;

        // Add local service to RIB
        self.rib.add_service(service_id.clone(), service).await;

        Ok(service_id)
    }

    async fn unregister(&self, service: Arc<Service>) {
        let service = self
            .services
            .write()
            .trace_expect("Failed to lock mutex")
            .remove(&service.service_id);

        if let Some(service) = service {
            self.unregister_service(service).await
        }
    }

    async fn unregister_service(&self, service: Arc<Service>) {
        // Remove local service from RIB
        self.rib.remove_service(&service.service_id, &service);

        service.service.on_unregister().await;

        info!("Unregistered service: {}", service.service_id);
    }

    pub async fn find(&self, service_id: &Eid) -> Option<Arc<Service>> {
        self.services
            .read()
            .trace_expect("Failed to lock mutex")
            .get(service_id)
            .cloned()
    }
}
