// The low-level service surface's wire: one Subscribe session per
// registration, its events translated onto the local
// [`Service`](services::Service) trait, and a sink whose calls are
// the wire's token-gated RPCs. Declarations are ordered
// define-before-reference: the collection stream, the sink, the
// delivery runner, the event loop, then the handshake. The registration itself lives on
// `BpaClient`: it opens the session here, hands the sink to the
// service, and drives the event loop.

use core::num::NonZeroUsize;
use std::sync::Arc;

use hardy_async::{BoundedTaskPool, CancellationToken};
use hardy_bpa::{
    Bytes, async_trait, services,
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle,
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
    service::{
        Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse, Register, SendMetadata,
        SendRequest, SubscribeRequest, SubscribeResponse, Unregister, receive_request, register,
        send_request, service_service_client::ServiceServiceClient, subscribe_request,
        subscribe_response,
    },
};

// The registration's sink: every call is a token-gated RPC of the
// session. No Drop impl is needed: dropping the sink drops the
// request sender, which half-closes the session stream, and the BPA
// treats that exactly as an Unregister.
pub struct GrpcServiceSink {
    client: ServiceServiceClient<Channel>,
    token: Bytes,
    requests: Sender<SubscribeRequest>,
}

#[async_trait]
impl services::ServiceSink for GrpcServiceSink {
    async fn unregister(&self) {
        let _ = self
            .requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await;
    }

    async fn send(&self, stream: &mut dyn Receiver<Segment>) -> services::Result<bundle::Id> {
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
        let response = response
            .map_err(|status| match status.code() {
                // The same error a local streamed send returns when
                // its producer gives up before the final segment.
                Code::Cancelled => services::Error::StreamCancelled,
                _ => service_error(status),
            })?
            .into_inner();
        bundle::Id::from_key(&response.bundle_id).map_err(|e| services::Error::Internal(e.into()))
    }
}

// The service surface's Receive door: how the generic [`Collector`]
// opens a collection against this surface's generated client.
#[async_trait]
impl ReceiveDoor for ServiceServiceClient<Channel> {
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
    service: Arc<dyn services::Service>,
    cancel: CancellationToken,
    bundle_id: bundle::Id,
    expiry: OffsetDateTime,
    delivery: Delivery,
    mut stream: adapter::Reader<ReceiveResponse, ReceiveRequest>,
) {
    let on_deliver = service.on_deliver(&bundle_id, expiry, delivery.bundle_size, &mut stream);
    // Completion is polled first so a delivery that finished in the same
    // instant the session ended is honoured; a pending one yields to the
    // teardown immediately.
    let result = tokio::select! {
        biased;
        result = on_deliver => result,
        _ = cancel.cancelled() => Err(services::Error::StreamCancelled),
    };
    let Err(e) = result else {
        return;
    };
    // Dropping `stream` short of completion abandons the collection with
    // the wire's in-band cancel (see `adapter::Reader`'s Drop), and the
    // bundle stays parked for a later attempt.
    log_declined("Service", &delivery.bundle_id, stream.is_complete(), &e);
}

// The session's event loop: wire events land on the local trait, and
// the session ending, however it ends (the stream closing or the
// client's shutdown), is the unregistration; malformed events are
// logged and skipped. Each delivery collects on its own task, bounded
// by [`MAX_CONCURRENT_DELIVERIES`], so the announcement loop keeps
// pulling while collections run.
pub async fn run_session(
    mut events: Streaming<SubscribeResponse>,
    collector: Collector<ServiceServiceClient<Channel>>,
    service: Arc<dyn services::Service>,
    cancel: CancellationToken,
) {
    // Every in-flight delivery races `session_cancel`, which fires on the
    // client's shutdown (it is a child of `cancel`) and at this session's
    // own end.
    let session_cancel = cancel.child_token();
    let deliveries = BoundedTaskPool::new(MAX_CONCURRENT_DELIVERIES);
    while let Some(SubscribeResponse { event }) = next_event(&mut events, &cancel).await {
        let Some(event) = event else {
            warn!("Ignoring event with no payload");
            continue;
        };
        match event {
            subscribe_response::Event::Registration(registration) => {
                warn!("Ignoring unexpected event: {registration:?}")
            }
            subscribe_response::Event::Delivery(delivery) => {
                let Ok(bundle_id) = bundle::Id::from_key(&delivery.bundle_id) else {
                    warn!("Ignoring delivery with invalid bundle id: {delivery:?}");
                    continue;
                };
                let Some(expiry) = delivery.expire_time.and_then(from_timestamp) else {
                    warn!("Ignoring delivery with invalid expiry: {delivery:?}");
                    continue;
                };
                let stream = collector.open(delivery.bundle_id.clone());
                let service = service.clone();
                let cancel = session_cancel.clone();
                hardy_async::spawn!(deliveries, "service_delivery", async move {
                    deliver(service, cancel, bundle_id, expiry, delivery, stream).await
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
                service
                    .on_status_notify(&bundle_id, &from, kind, reason, timestamp)
                    .await;
            }
        }
    }
    // In-flight deliveries end before the component learns it is
    // unregistered, so no `on_deliver` call outlives `on_unregister`.
    session_cancel.cancel();
    deliveries.shutdown().await;
    service.on_unregister().await;
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
    GrpcServiceSink,
    Collector<ServiceServiceClient<Channel>>,
    Streaming<SubscribeResponse>,
)> {
    let mut client = ServiceServiceClient::new(channel)
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
        GrpcServiceSink {
            client,
            token: registration.session_token,
            requests,
        },
        collector,
        events,
    ))
}
