use super::*;

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

    pub fn close_queue(&self) {
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
