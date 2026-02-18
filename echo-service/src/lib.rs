use hardy_async::sync::spin::Once;
use hardy_bpa::async_trait;
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
        // Ensure single initialization
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        // Do nothing
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
    /// - `data`: raw bundle bytes (service can parse if needed)
    /// - `expiry`: calculated from bundle metadata by dispatcher
    async fn on_receive(&self, data: hardy_bpa::Bytes, _expiry: time::OffsetDateTime) {
        _ = self.echo(data).await;
    }
}
