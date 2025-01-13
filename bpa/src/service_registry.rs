use super::*;
use rand::distributions::{Alphanumeric, DistString};
use rand::Rng;
use std::collections::HashMap;
use tokio::sync::RwLock;

struct Sink {
    registry: Arc<ServiceRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    eid: bpv7::Eid,
}

#[async_trait]
impl service::Sink for Sink {
    async fn disconnect(&self) {
        self.registry.unregister(&self.eid).await
    }

    async fn send(
        &self,
        destination: bpv7::Eid,
        data: &[u8],
        lifetime: time::Duration,
        flags: Option<service::SendFlags>,
    ) -> service::Result<bpv7::BundleId> {
        self.dispatcher
            .local_dispatch(self.eid.clone(), destination, data, lifetime, flags)
            .await
            .map_err(Into::into)
    }

    async fn collect(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> service::Result<Option<service::Bundle>> {
        self.dispatcher
            .collect(&self.eid, bundle_id)
            .await
            .map_err(Into::into)
    }
}

pub struct Service {
    service: Arc<dyn service::Service>,
    connected: connected::ConnectedFlag,
}

impl Service {
    pub async fn on_received(&self, bundle_id: &bpv7::BundleId, expiry: time::OffsetDateTime) {
        if self.connected.is_connected() {
            self.service.on_received(bundle_id, expiry).await
        }
    }

    pub async fn on_status_notify(
        &self,
        bundle_id: &bpv7::BundleId,
        kind: service::StatusNotify,
        reason: bpv7::StatusReportReasonCode,
        timestamp: Option<bpv7::DtnTime>,
    ) {
        if self.connected.is_connected() {
            self.service
                .on_status_notify(bundle_id, kind, reason, timestamp)
                .await
        }
    }
}

pub struct ServiceRegistry {
    admin_endpoints: Arc<admin_endpoints::AdminEndpoints>,
    services: RwLock<HashMap<bpv7::Eid, Arc<Service>>>,
}

impl ServiceRegistry {
    pub fn new(admin_endpoints: Arc<admin_endpoints::AdminEndpoints>) -> Self {
        Self {
            admin_endpoints,
            services: Default::default(),
        }
    }

    pub async fn shutdown(&self) {
        for (eid, service) in self.services.write().await.drain() {
            service.connected.disconnect();

            service.service.on_disconnect();

            info!("Unregistered service: {}", eid);
        }
    }

    #[instrument(skip(self, service, dispatcher))]
    pub async fn register(
        self: &Arc<Self>,
        eid: Option<&service::ServiceName<'_>>,
        service: Arc<dyn service::Service>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> service::Result<()> {
        // Scope the lock
        let (service, eid) = {
            let mut services = self.services.write().await;

            // Compose EID
            let eid = match eid {
                Some(service::ServiceName::DtnService(s)) => {
                    let Some(node_id) = self.admin_endpoints.dtn_node_id() else {
                        return Err(service::Error::NoDtnNodeId);
                    };

                    if s.is_empty() {
                        let mut rng = rand::thread_rng();
                        loop {
                            let eid = format!(
                                "dtn://{node_id}/auto/{}",
                                Alphanumeric.sample_string(&mut rng, 16)
                            )
                            .parse()
                            .unwrap();
                            if !services.contains_key(&eid) {
                                break eid;
                            }
                        }
                    } else {
                        let eid = format!("dtn://{node_id}/s")
                            .parse()
                            .map_err(|e: bpv7::EidError| service::Error::Internal(e.into()))?;
                        if services.contains_key(&eid) {
                            return Err(service::Error::DtnServiceInUse(s.to_string()));
                        }
                        eid
                    }
                }
                Some(service::ServiceName::IpnService(s)) => {
                    let Some((allocator_id, node_number)) = self.admin_endpoints.ipn_node_id()
                    else {
                        return Err(service::Error::NoIpnNodeId);
                    };

                    if *s == 0 {
                        let mut rng = rand::thread_rng();
                        loop {
                            let eid = bpv7::Eid::Ipn {
                                allocator_id,
                                node_number,
                                service_number: rng.gen_range(0x10000..=u32::MAX),
                            };
                            if !services.contains_key(&eid) {
                                break eid;
                            }
                        }
                    } else {
                        let eid = bpv7::Eid::Ipn {
                            allocator_id,
                            node_number,
                            service_number: *s,
                        };
                        if services.contains_key(&eid) {
                            return Err(service::Error::IpnServiceInUse(*s));
                        }
                        eid
                    }
                }
                None => {
                    let mut rng = rand::thread_rng();
                    if let Some((allocator_id, node_number)) = self.admin_endpoints.ipn_node_id() {
                        loop {
                            let eid = bpv7::Eid::Ipn {
                                allocator_id,
                                node_number,
                                service_number: rng.gen_range(0x10000..=u32::MAX),
                            };
                            if !services.contains_key(&eid) {
                                break eid;
                            }
                        }
                    } else {
                        let Some(node_id) = self.admin_endpoints.dtn_node_id() else {
                            return Err(service::Error::NoDtnNodeId);
                        };
                        loop {
                            let eid = format!(
                                "dtn://{node_id}/auto/{}",
                                Alphanumeric.sample_string(&mut rng, 16)
                            )
                            .parse()
                            .unwrap();
                            if !services.contains_key(&eid) {
                                break eid;
                            }
                        }
                    }
                }
            };

            let service = Arc::new(Service {
                service,
                connected: connected::ConnectedFlag::default(),
            });
            services.insert(eid.clone(), service.clone());
            (service, eid)
        };

        if let Err(e) = self.start_service(&eid, service, dispatcher).await {
            // Connect failed
            self.services.write().await.remove(&eid);
            Err(e)
        } else {
            Ok(())
        }
    }

    async fn start_service(
        self: &Arc<Self>,
        eid: &bpv7::Eid,
        service: Arc<Service>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> service::Result<()> {
        service
            .service
            .on_connect(
                Box::new(Sink {
                    registry: self.clone(),
                    dispatcher: dispatcher.clone(),
                    eid: eid.clone(),
                }),
                eid,
            )
            .await?;

        info!("Registered new service: {}", &eid);

        service.connected.connect();

        // Now get all bundles ready for collection
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(metadata::BundleMetadata, bpv7::Bundle)>(16);

        let r = dispatcher.poll_for_collection(eid, tx);

        let service_cloned = service.service.clone();
        tokio::spawn(async move {
            while let Some((metadata, bundle)) = rx.recv().await {
                let bundle = bundle::Bundle { bundle, metadata };
                // Double check that we are returning something valid
                if let metadata::BundleStatus::CollectionPending = &bundle.metadata.status {
                    let expiry = bundle.expiry();
                    if expiry > time::OffsetDateTime::now_utc() {
                        service_cloned.on_received(&bundle.bundle.id, expiry).await;
                    }
                }
            }
        })
        .await
        .map_err(|e| service::Error::Internal(e.into()))?;

        r.await.map_err(Into::into)
    }

    #[instrument(skip(self))]
    async fn unregister(&self, eid: &bpv7::Eid) {
        if let Some(service) = self.services.write().await.remove(eid) {
            service.connected.disconnect();

            service.service.on_disconnect();

            info!("Unregistered service: {}", eid);
        }
    }

    pub async fn find(&self, eid: &bpv7::Eid) -> Option<Arc<Service>> {
        self.services.read().await.get(eid).cloned()
    }
}
