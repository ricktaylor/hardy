// The application surface's wire: one Subscribe session per
// registration, its events translated onto the local
// [`Application`](services::Application) trait, and a sink whose
// calls are the wire's token-gated RPCs. Declarations are ordered
// define-before-reference: the wire conversions, the collection
// stream, the sink, the delivery runner, the event loop, then the
// handshake. The
// registration itself lives on `BpaClient`: it opens the session
// here, hands the sink to the application, and drives the event
// loop.

use core::{num::NonZeroUsize, ops::ControlFlow, time::Duration};
use std::sync::Arc;

use hardy_async::{BoundedTaskPool, CancellationToken};
use hardy_bpa::{
    Bytes, async_trait, services,
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id,
    eid::{self, Eid, Service},
};
use time::OffsetDateTime;
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Streaming, transport::Channel};
use tracing::{debug, warn};

use super::super::{
    SUBSCRIBE_REQUEST_CAPACITY, TRANSFER_REQUEST_CAPACITY, adapter,
    collector::{Collector, ReceiveDoor},
};
use super::{decode_status_report, from_timestamp, log_declined, next_event, service_error};
use crate::{
    MAX_MESSAGE_SIZE,
    application::{
        Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse, Register, SendMetadata,
        SendRequest, SubscribeRequest, SubscribeResponse, Unregister,
        application_service_client::ApplicationServiceClient, receive_request, register,
        send_request, subscribe_request, subscribe_response,
    },
};

// The registration's sink: every call is a token-gated RPC of the
// session. No Drop impl is needed: dropping the sink drops the
// request sender, which half-closes the session stream, and the BPA
// treats that exactly as an Unregister.
pub struct GrpcApplicationSink {
    client: ApplicationServiceClient<Channel>,
    token: Bytes,
    requests: Sender<SubscribeRequest>,
}

#[async_trait]
impl services::ApplicationSink for GrpcApplicationSink {
    async fn unregister(&self) {
        let _ = self
            .requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await;
    }

    async fn send(
        &self,
        destination: Eid,
        lifetime: Duration,
        options: Option<services::SendOptions>,
        size_hint: Option<u64>,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<Id> {
        // tonic wants a `'static` request stream, and `stream` is
        // borrowed for this call: the pump bridges them, pulling
        // segments and pushing wire chunks while the call runs, both
        // driven by the same join. Dropping the sender after the last
        // chunk half-closes the request side.
        let (requests, rx) = mpsc::channel::<SendRequest>(TRANSFER_REQUEST_CAPACITY);
        let pump = async move {
            if requests
                .send(SendRequest {
                    request: Some(send_request::Request::Metadata(SendMetadata {
                        session_token: self.token.clone(),
                        destination: destination.to_string(),
                        lifetime: Some(prost_types::Duration {
                            seconds: i64::try_from(lifetime.as_secs()).unwrap_or(i64::MAX),
                            nanos: lifetime.subsec_nanos() as i32,
                        }),
                        options: options.map(Into::into),
                        // The caller's reassembly hint, forwarded to the
                        // server's Send door; absent when unknown.
                        adu_size: size_hint,
                    })),
                })
                .await
                .is_err()
            {
                return;
            }
            adapter::Writer::new(&requests).write_all(stream).await;
        };

        let mut client = self.client.clone();
        let ((), response) = tokio::join!(pump, client.send(ReceiverStream::new(rx)));
        let response = response.map_err(service_error)?.into_inner();
        Id::from_key(&response.bundle_id).map_err(|e| services::Error::Internal(e.into()))
    }
}

// The application surface's Receive door: how the generic [`Collector`]
// opens a collection against this surface's generated client.
#[async_trait]
impl ReceiveDoor for ApplicationServiceClient<Channel> {
    type Request = ReceiveRequest;
    type Response = ReceiveResponse;

    fn metadata(token: &Bytes, bundle_id: String) -> ReceiveRequest {
        ReceiveRequest {
            request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                session_token: token.clone(),
                bundle_id,
            })),
        }
    }

    async fn open(
        &self,
        requests: ReceiverStream<ReceiveRequest>,
    ) -> Option<Streaming<ReceiveResponse>> {
        match self.clone().receive(requests).await {
            Ok(response) => Some(response.into_inner()),
            Err(e) => {
                debug!("Receive stream failed: {e}");
                None
            }
        }
    }
}

// How many announced deliveries one registration collects at once: enough
// that one slow collection does not serialise the rest, small enough that
// a single registration cannot monopolise its connection. Beyond the
// bound, the announcement loop waits for a slot, which backpressures the
// session stream and through it the BPA, by design.
const MAX_CONCURRENT_DELIVERIES: NonZeroUsize = NonZeroUsize::new(4).unwrap();

// Runs one announced delivery to its end, racing the session's teardown
// so a stuck collection cannot hang the client's shutdown.
async fn deliver(
    application: Arc<dyn services::Application>,
    cancel: CancellationToken,
    bundle_id: Id,
    expiry: OffsetDateTime,
    delivery: Delivery,
    mut reader: adapter::Reader<ReceiveResponse, ReceiveRequest>,
) {
    let result = tokio::select! {
        biased;
        result = application.on_deliver(&bundle_id, expiry, delivery.ack_requested, delivery.adu_size, &mut reader) => result,
        _ = cancel.cancelled() => Err(services::Error::StreamCancelled),
    };
    match result {
        // Accepted without receiving to completion: an application bug.
        // No ack goes out (an early ack is a protocol violation), so the
        // bundle stays parked for re-delivery.
        Ok(()) if !reader.is_complete() => {
            warn!(
                "Application accepted delivery {} without receiving it in full; it will be re-delivered",
                delivery.bundle_id
            );
        }
        // The application received the whole ADU; acknowledge to commit the
        // delivery (the server finalizes the bundle on this). The handshake
        // still yields to teardown: an ack lost that way only parks the
        // bundle for a duplicate re-delivery, and must not stall shutdown
        // on a server that never closes the response.
        Ok(()) => {
            tokio::select! {
                biased;
                _ = reader.acknowledge() => {}
                _ = cancel.cancelled() => {}
            }
        }
        // Dropping `reader` short of completion abandons the collection
        // with the wire's in-band cancel (see `adapter::Reader`'s Drop),
        // and the bundle stays parked for a later attempt.
        Err(e) => log_declined("Application", &delivery.bundle_id, reader.is_complete(), &e),
    }
}

// The session's event loop: wire events land on the local trait;
// malformed events are logged and skipped. Each delivery collects on its
// own task, bounded by [`MAX_CONCURRENT_DELIVERIES`], so the announcement
// loop keeps pulling while collections run. Returns `Ok(())` when the
// session ends cleanly (the client's shutdown or a server half-close)
// and `Err` when the stream fails; the caller owns the component's
// `on_unregister`.
pub async fn run_session(
    mut events: Streaming<SubscribeResponse>,
    collector: Collector<ApplicationServiceClient<Channel>>,
    application: Arc<dyn services::Application>,
    cancel: CancellationToken,
) -> services::Result<()> {
    // Every in-flight delivery races `session_cancel`, which fires on the
    // client's shutdown (it is a child of `cancel`) and at this session's
    // own end.
    let session_cancel = cancel.child_token();
    let deliveries = BoundedTaskPool::new(MAX_CONCURRENT_DELIVERIES);
    let result = loop {
        let SubscribeResponse { event } = match next_event(&mut events, &cancel).await {
            ControlFlow::Continue(response) => response,
            ControlFlow::Break(None) => break Ok(()),
            ControlFlow::Break(Some(status)) => break Err(service_error(status)),
        };
        let Some(event) = event else {
            warn!("Ignoring event with no payload");
            continue;
        };
        match event {
            subscribe_response::Event::Registration(registration) => {
                warn!("Ignoring unexpected event: {registration:?}")
            }
            subscribe_response::Event::Delivery(delivery) => {
                let Ok(bundle_id) = Id::from_key(&delivery.bundle_id) else {
                    warn!("Ignoring delivery with invalid bundle id: {delivery:?}");
                    continue;
                };
                let Some(expiry) = delivery.expire_time.and_then(from_timestamp) else {
                    warn!("Ignoring delivery with invalid expiry: {delivery:?}");
                    continue;
                };
                let reader = collector.open(delivery.bundle_id.clone());
                let application = application.clone();
                let cancel = session_cancel.clone();
                hardy_async::spawn!(deliveries, "application_delivery", async move {
                    deliver(application, cancel, bundle_id, expiry, delivery, reader).await
                })
                .await;
            }
            subscribe_response::Event::BundleStatusReport(report) => {
                let Some(kind) = Option::<services::StatusNotify>::from(report.assertion()) else {
                    warn!("Ignoring status report with no assertion: {report:?}");
                    continue;
                };
                let Some((bundle_id, from, reason, timestamp)) = decode_status_report(
                    &report.bundle_id,
                    &report.reporting_node,
                    report.reason_code,
                    report.status_time,
                ) else {
                    warn!("Ignoring malformed status report: {report:?}");
                    continue;
                };
                application
                    .on_status_notify(&bundle_id, &from, kind, reason, timestamp)
                    .await;
            }
        }
    };
    // In-flight deliveries end before the session is declared over, so no
    // `on_deliver` call outlives the caller's `on_unregister`.
    session_cancel.cancel();
    deliveries.shutdown().await;
    result
}

// The Subscribe handshake: Register up, Registration down, and the
// session is live. Returns the endpoint the registration is bound
// to, its sink, and the event stream, which run_session drives from
// then on.
pub async fn subscribe(
    channel: Channel,
    service_id: Option<Service>,
) -> services::Result<(
    Eid,
    GrpcApplicationSink,
    Collector<ApplicationServiceClient<Channel>>,
    Streaming<SubscribeResponse>,
)> {
    let mut client = ApplicationServiceClient::new(channel)
        .max_encoding_message_size(MAX_MESSAGE_SIZE)
        .max_decoding_message_size(MAX_MESSAGE_SIZE);

    // The wire requires Register first, sent without waiting for
    // response headers.
    let (requests, rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
    let register = Register {
        service_id: service_id.map(|id| match id {
            Service::Ipn(n) => register::ServiceId::Ipn(n),
            Service::Dtn(name) => register::ServiceId::Dtn(name.to_string()),
        }),
    };
    requests
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(register)),
        })
        .await
        .map_err(|e| services::Error::Internal(e.into()))?;

    let mut events = client
        .subscribe(ReceiverStream::new(rx))
        .await
        .map_err(|status| match status.code() {
            // The same error a local registration returns.
            Code::AlreadyExists => services::Error::ServiceIdInUse(status.message().to_string()),
            _ => service_error(status),
        })?
        .into_inner();

    let Some(SubscribeResponse {
        event: Some(subscribe_response::Event::Registration(registration)),
    }) = events.message().await.map_err(service_error)?
    else {
        return Err(services::Error::Internal(
            "The first event must be Registration".into(),
        ));
    };
    let eid: Eid = registration
        .endpoint_id
        .parse()
        .map_err(|e: eid::Error| services::Error::Internal(e.into()))?;

    let collector = Collector::new(client.clone(), registration.session_token.clone());
    Ok((
        eid,
        GrpcApplicationSink {
            client,
            token: registration.session_token,
            requests,
        },
        collector,
        events,
    ))
}
