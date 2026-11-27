use alloc::vec::Vec;

use hardy_async::{closeable, sync::spin::Once};

use super::*;
use crate::services::{ApplicationEndpoint, ServiceEndpoint};

// One endpoint of a registration being built: its id, its resolved EID and
// the sink the factory and `on_register` share.
struct Pending {
    service_id: hardy_bpv7::eid::Service,
    eid: Eid,
    sink: Arc<Sink>,
}

impl ServiceRegistry {
    // Claims every id in `service_ids`, builds the component once at all
    // their endpoints, publishes each endpoint as its own registration, and
    // activates them. Configured and runtime registrations all arrive here,
    // a single id being a set of one. Returns the EIDs in `service_ids`
    // order.
    //
    // All or none: the ids are claimed together before the factory runs,
    // so a taken id refuses the set before anything is built, and the
    // factory runs outside the registry's lock.
    pub(super) async fn register(
        self: &Arc<Self>,
        service_ids: &[hardy_bpv7::eid::Service],
        factory: BoxFactory,
        node_ids: &node_ids::NodeIds,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Vec<Eid>> {
        let eids = resolve_endpoints(service_ids, node_ids)?;
        if let Err(taken) = self.services.claim_all(service_ids) {
            return Err(services::Error::ServiceIdInUse(taken.to_string()));
        }

        let pending: Vec<Pending> = service_ids
            .iter()
            .zip(&eids)
            .map(|(service_id, eid)| Pending {
                service_id: service_id.clone(),
                eid: eid.clone(),
                sink: self.sink(eid, rib, dispatcher),
            })
            .collect();
        let service = match build(factory, &pending) {
            Ok(service) => service,
            Err(e) => {
                self.services.release_all(service_ids);
                return Err(e);
            }
        };

        let registrations = self.publish(service, &pending, dispatcher)?;
        for ((registration, rx), endpoint) in registrations.into_iter().zip(pending) {
            endpoint
                .sink
                .service
                .call_once(|| Arc::downgrade(&registration));
            self.activate(registration, rx, endpoint.sink, rib, dispatcher)
                .await;
        }
        Ok(eids)
    }

    fn sink(
        self: &Arc<Self>,
        eid: &Eid,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> Arc<Sink> {
        Arc::new(Sink {
            service: Once::new(),
            eid: eid.clone(),
            registry: self.clone(),
            rib: rib.clone(),
            dispatcher: dispatcher.clone(),
        })
    }

    // Constructs every endpoint's registration complete — the delivery
    // queue and canonical EID are constructor arguments — then files them
    // all against their claims: nothing reachable is ever half-built, and
    // the RIB only ever sees a deliverable service. Each channel's
    // creation-time poll recovers any DeliverPending bundles left over from
    // a previous registration under the same EID.
    fn publish(
        &self,
        service: ServiceImpl,
        pending: &[Pending],
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) -> services::Result<Vec<(Arc<Service>, closeable::Receiver<bundle::Bundle>)>> {
        let registrations: Vec<_> = pending
            .iter()
            .map(|endpoint| {
                let (tx, rx) = dispatcher.new_delivery_channel(&endpoint.eid);
                let registration = Service::new(
                    service.clone(),
                    endpoint.service_id.clone(),
                    endpoint.eid.clone(),
                    tx,
                );
                (registration, rx)
            })
            .collect();
        let entries = registrations
            .iter()
            .map(|(registration, _)| (registration.service_id.clone(), registration.clone()))
            .collect();
        if !self.services.file_all(entries) {
            // A shutdown ran while the factory did and took the claims. No
            // registration was reached; close the channels they opened,
            // because each spawns its storage poller at creation and
            // dropping the sender does not close it.
            for (registration, _) in &registrations {
                registration.close_queue();
            }
            let ids: Vec<_> = pending.iter().map(|p| p.service_id.clone()).collect();
            self.services.release_all(&ids);
            return Err(services::Error::Disconnected);
        }
        Ok(registrations)
    }

    // Brings one published registration live: its delivery queue, its
    // route, then `on_register` with its own sink.
    async fn activate(
        self: &Arc<Self>,
        service: Arc<Service>,
        rx: closeable::Receiver<bundle::Bundle>,
        sink: Arc<Sink>,
        rib: &Arc<routing::Rib>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
    ) {
        let eid = service.eid().clone();
        dispatcher.start_delivery_queue(service.clone(), rx);

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
        info!("Registered service: {eid}");
    }
}

// Runs a factory once at every endpoint of its registration, handing it
// each endpoint's shared sink.
fn build(factory: BoxFactory, pending: &[Pending]) -> services::Result<ServiceImpl> {
    Ok(match factory {
        BoxFactory::Service(build) => {
            let endpoints = pending
                .iter()
                .map(|p| ServiceEndpoint {
                    eid: p.eid.clone(),
                    sink: p.sink.clone(),
                })
                .collect();
            ServiceImpl::LowLevel(build(ServiceContext::new(endpoints))?)
        }
        BoxFactory::Application(build) => {
            let endpoints = pending
                .iter()
                .map(|p| ApplicationEndpoint {
                    eid: p.eid.clone(),
                    sink: p.sink.clone(),
                })
                .collect();
            ServiceImpl::Application(build(ApplicationContext::new(endpoints))?)
        }
    })
}

// Resolves each id of a registration to its EID, refusing the set if it is
// empty, or if any id is the administrative endpoint, has no node id for
// its scheme, or appears twice.
pub(super) fn resolve_endpoints(
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
