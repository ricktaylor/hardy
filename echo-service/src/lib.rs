/*!
Bundle echo service for the Hardy BPA.

Implements a simple echo (ping) service: for each received bundle it builds a
fresh response bundle that reflects the payload back to the sender, conforming
to the BPv7 Echo Service specification (draft-taylor-dtn-echo-service). The
response is a freshly-sourced bundle (new creation timestamp) sourced from the
pinged endpoint and addressed to the request's source; only the payload is
reflected. The response is submitted to the BPA for normal forwarding.

# Key types

- [`EchoService`] — the service implementation, one instance per registered endpoint.
*/

use hardy_async::sync::spin::Once;
use hardy_bpa::async_trait;
use tracing::{debug, warn};

/// A BPA service that echoes received bundles back to their source.
///
/// When a bundle is delivered to a registered endpoint, the service builds a
/// fresh response bundle reflecting the payload back to the sender and submits
/// it to the BPA. This is the DTN equivalent of ICMP echo (ping).
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

    pub async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }

    async fn echo(&self, data: hardy_bpa::Bytes) -> Result<(), hardy_bpa::Error> {
        let Some(sink) = self.sink.get() else {
            return Ok(());
        };

        // Parse the request bundle.
        let bundle = hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys)
            .inspect_err(|e| debug!("Failed to parse incoming bundle: {e:?}"))?
            .bundle;

        // When a Response Is Sent: do not respond to a bundle with no return
        // path (null source), nor to an administrative record (which would risk
        // reflecting status reports and bundle loops).
        if bundle.id.source.is_null() {
            debug!("Not echoing bundle from the null endpoint");
            return Ok(());
        }
        if bundle.flags.is_admin_record {
            debug!("Not echoing administrative record");
            return Ok(());
        }

        // Reflect only the payload.
        let Some(payload) = bundle.blocks.get(&1).and_then(|block| block.payload(&data)) else {
            debug!("Incoming bundle has no payload block");
            return Ok(());
        };

        debug!(
            source = %bundle.id.source,
            destination = %bundle.destination,
            "Received bundle, building echo response"
        );

        // Build a fresh response bundle: source = the endpoint that was pinged
        // (the request's destination), destination = the request's source, with
        // a new creation timestamp. Adopt the request's lifetime (the BPA bounds
        // it by local policy as for any bundle).
        let mut builder =
            hardy_bpv7::builder::Builder::new(bundle.destination.clone(), bundle.id.source.clone())
                .with_lifetime(bundle.lifetime)
                .with_flags(response_flags(&bundle.flags));

        // If the request asked for status reports, direct the response's reports
        // to the same report-to so an observer can follow both legs of the
        // exchange. The matching request flags are mirrored in response_flags.
        if requested_status_reports(&bundle.flags) {
            builder = builder.with_report_to(bundle.report_to.clone());
        }

        let (_, response) = builder
            .with_payload(payload.into())
            .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
            .inspect_err(|e| debug!("Failed to build echo response: {e:?}"))?;

        debug!(
            source = %bundle.destination,
            destination = %bundle.id.source,
            "Sending echo response"
        );

        sink.send(response.into()).await.inspect_err(|e| {
            warn!("Failed to send echo response: {e:?}");
        })?;

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

// Whether the request asked for any kind of status report.
fn requested_status_reports(flags: &hardy_bpv7::bundle::Flags) -> bool {
    flags.receipt_report_requested
        || flags.forward_report_requested
        || flags.delivery_report_requested
        || flags.delete_report_requested
}

// The response bundle's processing control flags, derived from the request per
// draft-taylor-dtn-echo-service: reflect "bundle must not be fragmented", and —
// when the request asked for status reports — mirror its status-report-request
// flags and the "status time requested in reports" flag. Every other flag
// (notably administrative-record, fragment, and application-acknowledgement)
// takes the node-sourced default.
fn response_flags(request: &hardy_bpv7::bundle::Flags) -> hardy_bpv7::bundle::Flags {
    let mut flags = hardy_bpv7::bundle::Flags {
        do_not_fragment: request.do_not_fragment,
        ..Default::default()
    };
    if requested_status_reports(request) {
        flags.report_status_time = request.report_status_time;
        flags.receipt_report_requested = request.receipt_report_requested;
        flags.forward_report_requested = request.forward_report_requested;
        flags.delivery_report_requested = request.delivery_report_requested;
        flags.delete_report_requested = request.delete_report_requested;
    }
    flags
}
