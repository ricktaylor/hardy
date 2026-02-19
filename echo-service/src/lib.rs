use hardy_async::sync::spin::Once;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use std::sync::Arc;
use tracing::{debug, warn};

pub struct EchoService {
    sink: Once<Box<dyn hardy_bpa::services::ServiceSink>>,
}

impl Default for EchoService {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoService {
    pub fn new() -> Self {
        EchoService { sink: Once::new() }
    }

    /// Registers this service on the specified service IDs.
    ///
    /// The same EchoService instance can be registered on multiple service IDs.
    /// Only the first registration's sink is stored (subsequent registrations
    /// share the same underlying service).
    pub async fn register(
        self: &Arc<Self>,
        bpa: &dyn BpaRegistration,
        services: &[hardy_bpv7::eid::Service],
    ) -> Result<Vec<hardy_bpv7::eid::Eid>, hardy_bpa::services::Error> {
        let mut eids = Vec::with_capacity(services.len());
        for service in services {
            let eid = bpa
                .register_service(Some(service.clone()), self.clone())
                .await?;
            eids.push(eid);
        }
        Ok(eids)
    }

    /// Unregisters this service from the BPA.
    pub async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }

    async fn echo(&self, data: hardy_bpa::Bytes) -> Result<(), hardy_bpa::Error> {
        if let Some(sink) = self.sink.get() {
            // Parse the bundle
            let bundle = hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys)
                .inspect_err(|e| warn!("Failed to parse incoming bundle: {e:?}"))?
                .bundle;

            debug!(
                source = %bundle.id.source,
                destination = %bundle.destination,
                "Received bundle, reflecting back to source"
            );

            // Swap source and destination
            let data = hardy_bpv7::editor::Editor::new(&bundle, &data)
                .with_source(bundle.destination.clone())
                .map_err(|(_, e)| {
                    warn!("Failed to set source Eid: {e:?}");
                    e
                })?
                .with_destination(bundle.id.source.clone())
                .map_err(|(_, e)| {
                    warn!("Failed to set destination Eid: {e:?}");
                    e
                })?
                .rebuild()
                .inspect_err(|e| warn!("Failed to update bundle: {e:?}"))?;

            debug!(
                source = %bundle.destination,
                destination = %bundle.id.source,
                "Sending echo reply"
            );

            sink.send(data.into()).await.inspect_err(|e| {
                warn!("Failed to send reply: {e:?}");
            })?;
        }
        Ok(())
    }
}

#[async_trait]
impl hardy_bpa::services::Service for EchoService {
    async fn on_register(
        &self,
        _source: &hardy_bpv7::eid::Eid,
        sink: Box<dyn hardy_bpa::services::ServiceSink>,
    ) {
        // Store sink (only first registration succeeds, others are ignored)
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        // Nothing to clean up
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &hardy_bpv7::eid::Eid,
        _kind: hardy_bpa::services::StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
        // Do nothing
    }

    /// Called when a bundle arrives
    async fn on_receive(&self, data: hardy_bpa::Bytes, _expiry: time::OffsetDateTime) {
        _ = self.echo(data).await;
    }
}
