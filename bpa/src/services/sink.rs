// The per-endpoint sink a registered service or application sends
// through: it originates bundles from its own endpoint only, and dropping
// it unregisters that endpoint.

use hardy_async::async_trait;
use hardy_bpv7::eid::Eid;
use tracing::error;

use super::registry::{Service, ServiceRegistry};
use crate::{Arc, Bytes, Weak, dispatcher, routing, services};

// Sink implementation for both Service and Application traits
pub struct Sink {
    pub service: Weak<Service>,
    // Full EID for this registration (pre-resolved at activation time)
    pub eid: Eid,
    pub registry: Arc<ServiceRegistry>,
    pub rib: Arc<routing::Rib>,
    pub dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Sink {
    async fn unregister_inner(&self) {
        if let Some(service) = self.service.upgrade()
            && let Err(e) = self.registry.unregister(service, &self.rib).await
        {
            error!("Failed to unregister service: {e}");
        }
    }
}

#[async_trait]
impl services::ServiceSink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(
        &self,
        stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
    ) -> services::Result<hardy_bpv7::bundle::Id> {
        let service = self
            .service
            .upgrade()
            .ok_or(services::Error::Disconnected)?;

        // A service that unregisters mid-send must not originate its bundle:
        // the registration's token races every pull, so teardown wakes this
        // stream immediately — even parked behind a stalled producer — and
        // the send surfaces as cancelled. Sink-side, so the dispatcher's
        // stream consumers stay registration-agnostic.
        let mut stream = crate::stream::CancellableReceiver {
            inner: stream,
            token: service.cancel.clone(),
        };
        self.dispatcher.originate_raw(&self.eid, &mut stream).await
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
        self.service
            .upgrade()
            .ok_or(services::Error::Disconnected)?;

        self.dispatcher
            .originate(self.eid.clone(), destination, data, lifetime, options)
            .await
    }

    async fn send_streamed(
        &self,
        destination: Eid,
        total_len: u64,
        stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
        lifetime: core::time::Duration,
        options: Option<services::SendOptions>,
    ) -> services::Result<hardy_bpv7::bundle::Id> {
        let service = self
            .service
            .upgrade()
            .ok_or(services::Error::Disconnected)?;

        // Registration teardown wakes an in-flight send exactly as on the
        // raw door: the token races every pull, sink-side.
        let mut stream = crate::stream::CancellableReceiver {
            inner: stream,
            token: service.cancel.clone(),
        };
        self.dispatcher
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
        if let Some(service) = self.service.upgrade() {
            let registry = self.registry.clone();
            let rib = self.rib.clone();
            hardy_async::spawn!(self.registry.tasks, "sink_drop_cleanup", async move {
                if let Err(e) = registry.unregister(service, &rib).await {
                    error!("Failed to unregister service: {e}");
                }
            });
        }
    }
}
