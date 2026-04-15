//! Bundle echo service for the Hardy BPA.
//!
//! Implements a simple echo (ping) service that reflects received bundles back
//! to their source, as defined by the BPv7 bundle delivery model (RFC 9171 §5.6).
//! Each incoming bundle is re-built with the source and destination swapped, then
//! injected back into the BPA for forwarding.
//!
//! # Key types
//!
//! - [`EchoService`] — the service implementation, one instance per registered endpoint.

use hardy_async::sync::spin::Once;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use std::sync::Arc;
use tracing::{debug, warn};

/// A BPA service that echoes received bundles back to their source.
///
/// When a bundle is delivered to a registered endpoint, the service swaps the
/// source and destination EIDs and sends the bundle back through the BPA.
/// This is the DTN equivalent of ICMP echo (ping).
pub struct EchoService {
    /// Communication channel back to the BPA, set once during registration.
    sink: Once<Box<dyn hardy_bpa::services::ServiceSink>>,
}

impl Default for EchoService {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoService {
    /// Creates a new `EchoService` with no BPA sink attached.
    pub fn new() -> Self {
        EchoService { sink: Once::new() }
    }

    /// Registers this service on the specified service IDs.
    ///
    /// Creates a separate EchoService instance per endpoint to work around
    /// the single-sink-per-registration API limitation (see #479).
    pub async fn register(
        bpa: &dyn BpaRegistration,
        services: &[hardy_bpv7::eid::Service],
    ) -> Result<Vec<hardy_bpv7::eid::Eid>, hardy_bpa::services::Error> {
        let mut eids = Vec::with_capacity(services.len());
        for service in services {
            let echo = Arc::new(Self::new());
            let eid = bpa.register_service(Some(service.clone()), echo).await?;
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
                .inspect_err(|e| debug!("Failed to parse incoming bundle: {e:?}"))?
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
                    debug!("Failed to set source Eid: {e:?}");
                    e
                })?
                .with_destination(bundle.id.source.clone())
                .map_err(|(_, e)| {
                    debug!("Failed to set destination Eid: {e:?}");
                    e
                })?
                .rebuild()
                .inspect_err(|e| debug!("Failed to update bundle: {e:?}"))?;

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
    /// Stores the BPA sink for later use when echoing bundles.
    async fn on_register(
        &self,
        _source: &hardy_bpv7::eid::Eid,
        sink: Box<dyn hardy_bpa::services::ServiceSink>,
    ) {
        self.sink.call_once(|| sink);
    }

    /// No-op; no resources to release beyond the sink itself.
    async fn on_unregister(&self) {
        // Nothing to clean up
    }

    /// No-op; the echo service does not act on status reports.
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
