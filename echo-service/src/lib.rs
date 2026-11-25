//! Bundle echo service for the Hardy BPA, per the BPv7 Echo Service
//! specification (draft-taylor-dtn-echo-service).
//!
//! [`EchoService`], one instance per registered endpoint, answers each
//! received bundle with a fresh response: a new creation timestamp, the
//! pinged endpoint as its source, the request's source as its destination,
//! and the request's payload reflected. The response goes to the BPA for
//! ordinary forwarding.

use std::sync::Arc;

use hardy_bpa::{
    async_trait,
    service::{self, StatusNotify, raw},
    stream::{Receiver, Segment, buffer_stream},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::{Flags, Id},
    creation_timestamp::CreationTimestamp,
    eid::Eid,
    parse::{Parsed, parse},
    status_report::ReasonCode,
};
use tracing::{debug, warn};
/// A BPA service that echoes received bundles back to their source.
///
/// When a bundle is delivered to a registered endpoint, the service builds a
/// fresh response bundle reflecting the payload back to the sender and submits
/// it to the BPA. This is the DTN equivalent of ICMP echo (ping).
pub struct EchoService {
    /// Communication channel back to the BPA, for the service's whole
    /// active lifetime.
    sink: Arc<dyn raw::Sink>,
}

impl EchoService {
    /// A factory for an echo service at whichever endpoint the BPA
    /// settles on.
    pub fn factory() -> impl raw::Factory<Component = Self> {
        raw::factory_fn(|cx: raw::Context| Ok(Arc::new(Self { sink: cx.sink })))
    }

    /// Unregisters the service from the BPA.
    pub async fn unregister(&self) {
        self.sink.unregister().await;
    }

    async fn echo(&self, data: hardy_bpa::Bytes) -> Result<(), hardy_bpa::Error> {
        // Parse the request bundle. A malformed bundle is a permanent failure:
        // stop rather than propagate, which would park it for a retry that can
        // never succeed. Only a failed send (below) is transient and worth a
        // retry. In practice the BPA already parsed these bytes at ingress.
        let Ok(Parsed { data, bundle, .. }) =
            parse(data).inspect_err(|e| debug!("Failed to parse incoming bundle: {e:?}"))
        else {
            return Ok(());
        };

        // No response goes to a bundle with no return path, its source
        // null, nor to an administrative record, which would risk
        // reflecting status reports and looping bundles.
        if bundle.primary.id.source.is_null() {
            debug!("Not echoing bundle from the null endpoint");
            return Ok(());
        }
        if bundle.primary.flags.is_admin_record {
            debug!("Not echoing administrative record");
            return Ok(());
        }

        // Reflect only the payload.
        let Some(payload) = bundle.blocks.get(&1).and_then(|block| block.payload(&data)) else {
            debug!("Incoming bundle has no payload block");
            return Ok(());
        };

        debug!(
            source = %bundle.primary.id.source,
            destination = %bundle.primary.destination,
            "Received bundle, building echo response"
        );

        // Build a fresh response bundle: source = the endpoint that was pinged
        // (the request's destination), destination = the request's source, with
        // a new creation timestamp. Adopt the request's lifetime (the BPA bounds
        // it by local policy as for any bundle).
        let mut builder = Builder::new(
            bundle.primary.destination.clone(),
            bundle.primary.id.source.clone(),
        )
        .with_lifetime(bundle.primary.lifetime)
        .with_flags(response_flags(&bundle.primary.flags));

        // If the request asked for status reports, direct the response's reports
        // to the same report-to so an observer can follow both legs of the
        // exchange. The matching request flags are mirrored in response_flags.
        if requested_status_reports(&bundle.primary.flags) {
            builder = builder.with_report_to(bundle.primary.report_to.clone());
        }

        // Building from an already-parsed bundle is deterministic, so a failure
        // here is permanent too: stop rather than park for retry.
        let Ok((_, response)) = builder
            .with_payload(payload.into())
            .build(CreationTimestamp::now())
            .inspect_err(|e| debug!("Failed to build echo response: {e:?}"))
        else {
            return Ok(());
        };

        debug!(
            source = %bundle.primary.destination,
            destination = %bundle.primary.id.source,
            "Sending echo response"
        );

        self.sink
            .send(&mut hardy_bpa::Bytes::from(response))
            .await
            .inspect_err(|e| {
                warn!("Failed to send echo response: {e:?}");
            })?;

        Ok(())
    }
}

#[async_trait]
impl raw::Service for EchoService {
    /// Discards the notification: the echo service does not act on status
    /// reports.
    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }

    // The echo service parses the whole request bundle with a whole-buffer
    // codec, so `stream::buffer_stream` assembles the stream in memory
    // before the payload is reflected. See
    // bpa/docs/streaming_pipeline_design.md for the streaming form this is
    // a step towards.
    async fn deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        let data = buffer_stream(stream, total_len).await?;
        self.echo(data).await.map_err(Into::into)
    }
}

// Whether the request asked for any kind of status report.
fn requested_status_reports(flags: &Flags) -> bool {
    flags.receipt_report_requested
        || flags.forward_report_requested
        || flags.delivery_report_requested
        || flags.delete_report_requested
}

// The response bundle's processing control flags, derived from the request
// per draft-taylor-dtn-echo-service: "bundle must not be fragmented" is
// reflected, and a request that asked for status reports also has its
// status-report-request flags and its "status time requested in reports"
// flag mirrored. Every other flag, administrative-record, fragment and
// application-acknowledgement among them, takes the node-sourced default.
fn response_flags(request: &Flags) -> Flags {
    let mut flags = Flags {
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
