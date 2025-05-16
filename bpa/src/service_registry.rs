use super::*;
use rand::{
    Rng,
    distr::{Alphanumeric, SampleString},
};
use std::{collections::HashMap, sync::Weak};
use tokio::sync::RwLock;

pub struct Service {
    pub service: Arc<dyn service::Service>,
    pub service_id: bpv7::Eid,
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
        destination: bpv7::Eid,
        data: &[u8],
        lifetime: time::Duration,
        flags: Option<service::SendFlags>,
    ) -> service::Result<Box<str>> {
        let Some(service) = self.service.upgrade() else {
            return Err(service::Error::Disconnected);
        };

        // Sanity check
        if let bpv7::Eid::Null = &destination {
            return Err(service::Error::InvalidDestination(destination));
        }

        self.dispatcher
            .local_dispatch(
                service.service_id.clone(),
                destination,
                data,
                lifetime,
                flags,
            )
            .await
            .map(|bundle_id| bundle_id.to_key().into())
            .map_err(Into::into)
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
    rib: Arc<rib::Rib>,
    services: RwLock<HashMap<bpv7::Eid, Arc<Service>>>,
}

impl ServiceRegistry {
    pub fn new(rib: Arc<rib::Rib>) -> Self {
        Self {
            rib,
            services: Default::default(),
        }
    }

    pub async fn shutdown(&self) {
        for service in self
            .services
            .write()
            .await
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
        {
            self.unregister_service(service).await
        }
    }

    #[instrument(skip(self, service, dispatcher))]
    pub async fn register(
        self: &Arc<Self>,
        service_id: Option<service::ServiceId<'_>>,
        service: Arc<dyn service::Service>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> service::Result<bpv7::Eid> {
        // Scope the lock
        let (service, service_id) = {
            let mut services = self.services.write().await;

            let new_ipn_service = |allocator_id, node_number| {
                let mut rng = rand::rng();
                loop {
                    let service_id = bpv7::Eid::Ipn {
                        allocator_id,
                        node_number,
                        service_number: rng.random_range(0x10000..=u32::MAX),
                    };
                    if !services.contains_key(&service_id) {
                        break service_id;
                    }
                }
            };
            let new_dtn_service = |node_name: &str| {
                let mut rng = rand::rng();
                loop {
                    let service_id = bpv7::Eid::Dtn {
                        node_name: node_name.into(),
                        demux: [
                            "auto".into(),
                            Alphanumeric.sample_string(&mut rng, 16).into(),
                        ]
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
                    service::ServiceId::DtnService(service_name) => {
                        let Some(node_name) = &dispatcher.node_ids.dtn else {
                            return Err(service::Error::NoDtnNodeId);
                        };

                        if service_name.is_empty() {
                            new_dtn_service(node_name)
                        } else {
                            // Round-trip via Eid for formatting sanity
                            let bpv7::Eid::Dtn {
                                node_name: _,
                                demux,
                            } = format!("dtn://nowhere/{service_name}")
                                .parse::<bpv7::Eid>()
                                .map_err(|e| service::Error::Internal(e.into()))?
                            else {
                                panic!("DTN scheme parsing is borked!");
                            };

                            let service_id = bpv7::Eid::Dtn {
                                node_name: node_name.clone(),
                                demux,
                            };
                            if services.contains_key(&service_id) {
                                return Err(service::Error::DtnServiceInUse(
                                    service_name.to_string(),
                                ));
                            }
                            service_id
                        }
                    }
                    service::ServiceId::IpnService(service_number) => {
                        let Some((allocator_id, node_number)) = dispatcher.node_ids.ipn else {
                            unreachable!()
                        };

                        if service_number == &0 {
                            new_ipn_service(allocator_id, node_number)
                        } else {
                            let service_id = bpv7::Eid::Ipn {
                                allocator_id,
                                node_number,
                                service_number: *service_number,
                            };
                            if services.contains_key(&service_id) {
                                return Err(service::Error::IpnServiceInUse(*service_number));
                            }
                            service_id
                        }
                    }
                }
            } else if let Some((allocator_id, node_number)) = dispatcher.node_ids.ipn {
                new_ipn_service(allocator_id, node_number)
            } else if let Some(node_name) = &dispatcher.node_ids.dtn {
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
        self.rib.add_local(service_id.clone().into(), service).await;

        Ok(service_id)
    }

    async fn unregister(&self, service: Arc<Service>) {
        if let Some(service) = self.services.write().await.remove(&service.service_id) {
            self.unregister_service(service).await
        }
    }

    async fn unregister_service(&self, service: Arc<Service>) {
        // Remove local service from RIB
        self.rib
            .remove_local(&service.service_id.clone().into(), &service)
            .await;

        service.service.on_unregister().await;

        info!("Unregistered service: {}", service.service_id);
    }

    pub async fn find(&self, service_id: &bpv7::Eid) -> Option<Arc<Service>> {
        self.services.read().await.get(service_id).cloned()
    }
}
