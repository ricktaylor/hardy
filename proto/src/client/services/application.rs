// The application surface's wire: one Subscribe session per
// registration, its events translated onto the local
// [`Application`](services::Application) trait, and a sink whose
// calls are the wire's token-gated RPCs. Declarations are ordered
// define-before-reference: the wire conversions, the collection
// stream, the sink, the event loop, then the handshake. The
// registration itself lives on `BpaClient`: it opens the session
// here, hands the sink to the application, and drives the event
// loop.

use core::time::Duration;
use std::sync::Arc;

use hardy_async::CancellationToken;
use hardy_bpa::{
    Bytes, async_trait, services,
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id,
    eid::{self, Eid, Service},
};
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Streaming, transport::Channel};
use tracing::{debug, warn};

use crate::MAX_MESSAGE_SIZE;
use crate::application::{
    ReceiveMetadata, ReceiveRequest, ReceiveResponse, Register, SendMetadata, SendRequest,
    SubscribeRequest, SubscribeResponse, Unregister,
    application_service_client::ApplicationServiceClient, receive_request, register, send_request,
    subscribe_request, subscribe_response,
};

use super::super::adapter;
use super::super::collector::{Collector, ReceiveDoor};
use super::{decode_status_report, from_timestamp, service_error};

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
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<Id> {
        // tonic wants a `'static` request stream, and `stream` is
        // borrowed for this call: the pump bridges them, pulling
        // segments and pushing wire chunks while the call runs, both
        // driven by the same join. Dropping the sender after the last
        // chunk half-closes the request side.
        let (requests, rx) = mpsc::channel::<SendRequest>(2);
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
                        // Unknown until the stream completes: the
                        // declared size is only ever a hint.
                        adu_size: None,
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
        self.clone()
            .receive(requests)
            .await
            .ok()
            .map(|response| response.into_inner())
    }
}

// The session's event loop: wire events land on the local trait, and
// the session ending, however it ends (the stream closing or the
// client's shutdown), is the unregistration; malformed events are
// logged and skipped. Events are handled sequentially: a slow
// `on_deliver` backpressures the session stream, and through it the
// BPA, by design.
pub async fn run_session(
    mut events: Streaming<SubscribeResponse>,
    collector: Collector<ApplicationServiceClient<Channel>>,
    application: Arc<dyn services::Application>,
    cancel: CancellationToken,
) {
    loop {
        let message = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            message = events.message() => message,
        };
        let event = match message {
            Ok(Some(SubscribeResponse { event: Some(event) })) => event,
            Ok(Some(msg)) => {
                warn!("Ignoring empty event: {msg:?}");
                continue;
            }
            Ok(None) | Err(_) => break,
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
                let mut stream = collector.open(delivery.bundle_id.clone());
                if let Err(e) = application
                    .on_deliver(
                        &bundle_id,
                        expiry,
                        delivery.ack_requested,
                        delivery.adu_size,
                        &mut stream,
                    )
                    .await
                {
                    debug!("Application declined delivery {}: {e}", delivery.bundle_id);
                }
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
    }
    application.on_unregister().await;
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
    let (requests, rx) = mpsc::channel(4);
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
