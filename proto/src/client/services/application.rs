// Client of the application wire surface: one Subscribe session per
// registration.

use core::{ops::ControlFlow, time::Duration};
use std::sync::Arc;

use hardy_async::{BoundedTaskPool, CancellationToken};
use hardy_bpa::{
    async_trait,
    services::{Application, ApplicationSink, Error, Result, SendOptions, StatusNotify},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id as BundleId,
    eid::{self, Eid, Service},
    status_report::ReasonCode,
};
use time::OffsetDateTime;
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Streaming, transport::Channel};
use tracing::{debug, warn};

use super::{LocalUnregister, next_event, service_error, session_error};
use crate::{
    MAX_MESSAGE_SIZE,
    application::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, Register, SendMetadata,
        SendRequest, SubscribeRequest, SubscribeResponse, Unregister,
        application_service_client::ApplicationServiceClient, receive_request, register,
        send_request, subscribe_request, subscribe_response,
    },
    client::{
        MAX_CONCURRENT_DELIVERIES, SUBSCRIBE_REQUEST_CAPACITY, TRANSFER_REQUEST_CAPACITY,
        adapter::ResponseReader, write_transfer,
    },
    grammar::{Ack, Cancel},
    timestamp::from_timestamp,
    token::Token,
};

// Dropping the sink drops `requests_tx`, half-closing the session
// stream; the BPA treats that as an Unregister, so `Drop` records the
// ending as locally requested.
pub struct GrpcApplicationSink {
    client: ApplicationServiceClient<Channel>,
    token: Token,
    requests_tx: Sender<SubscribeRequest>,
    local_unregister: Arc<LocalUnregister>,
}

impl Drop for GrpcApplicationSink {
    fn drop(&mut self) {
        self.local_unregister.record();
    }
}

#[async_trait]
impl ApplicationSink for GrpcApplicationSink {
    async fn unregister(&self) {
        self.local_unregister.record();
        let _ = self
            .requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await;
    }

    async fn send(
        &self,
        destination: Eid,
        lifetime: Duration,
        options: Option<SendOptions>,
        size_hint: Option<u64>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<BundleId> {
        let metadata = SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: self.token.to_bytes(),
                destination: destination.to_string(),
                // An out-of-range lifetime is an error, not clamped.
                lifetime: Some(prost_types::Duration {
                    seconds: i64::try_from(lifetime.as_secs())
                        .map_err(|_| Error::Internal("Bundle lifetime out of range".into()))?,
                    nanos: lifetime.subsec_nanos() as i32,
                }),
                options: options.map(Into::into),
                adu_size: size_hint,
            })),
        };
        let mut client = self.client.clone();
        let response = write_transfer(metadata, stream, |requests| client.send(requests))
            .await
            .map_err(service_error)?
            .into_inner();
        BundleId::from_key(&response.bundle_id).map_err(|e| Error::Internal(e.into()))
    }
}

// Session state shared by the event loop and its delivery tasks.
struct ApplicationSessionCtx {
    application: Arc<dyn Application>,
    client: ApplicationServiceClient<Channel>,
    token: Token,
    // Child of the caller's token; cancelling it ends the event loop
    // and the delivery tasks.
    cancel: CancellationToken,
    // Distinguishes a local unregister from the BPA ending the session.
    local_unregister: Arc<LocalUnregister>,
}

impl ApplicationSessionCtx {
    fn new(
        application: Arc<dyn Application>,
        client: ApplicationServiceClient<Channel>,
        token: Token,
        cancel: CancellationToken,
        local_unregister: Arc<LocalUnregister>,
    ) -> Arc<Self> {
        Arc::new(Self {
            application,
            client,
            token,
            cancel: cancel.child_token(),
            local_unregister,
        })
    }

    // Collects one announced bundle over its own Receive call.
    async fn deliver(
        self: Arc<Self>,
        bundle_id: BundleId,
        expiry: OffsetDateTime,
        delivery: Delivery,
    ) {
        let mut client = self.client.clone();

        // The bundle stays parked on the server until it is collected
        // and acked, so every failure path here simply returns.
        let (requests_tx, requests_rx) = mpsc::channel::<ReceiveRequest>(TRANSFER_REQUEST_CAPACITY);
        if requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: self.token.to_bytes(),
                    bundle_id: delivery.bundle_id.clone(),
                })),
            })
            .await
            .is_err()
        {
            return;
        }
        let mut responses = tokio::select! {
            biased;
            result = client.receive(ReceiverStream::new(requests_rx)) => match result {
                Ok(call) => call.into_inner(),
                Err(status) => {
                    warn!("Collecting delivery {} failed: {status}", delivery.bundle_id);
                    return;
                }
            },
            _ = self.cancel.cancelled() => return,
        };

        let mut reader = ResponseReader::new(&mut responses);
        // Biased: a completed delivery is settled even when
        // cancellation is also pending.
        let result = tokio::select! {
            biased;
            result = self.application.on_deliver(
                &bundle_id,
                expiry,
                delivery.ack_requested,
                delivery.adu_size,
                &mut reader,
            ) => result,
            _ = self.cancel.cancelled() => Err(Error::StreamCancelled),
        };

        match result {
            // Await the server's close: dropping the call while the
            // Ack is in flight resets the stream and loses the ack.
            Ok(()) => {
                tokio::select! {
                    biased;
                    _ = async {
                        if requests_tx.send(ReceiveRequest::ack()).await.is_ok() {
                            match responses.message().await {
                                Ok(None) => {}
                                Err(status) => debug!("Collection stream ended with an error: {status}"),
                                Ok(Some(_)) => debug!("A message arrived where the response close was due"),
                            }
                        }
                    } => {}
                    _ = self.cancel.cancelled() => {}
                }
            }
            // An in-band Cancel tells the server to park the bundle.
            Err(e) => {
                debug!("Application declined delivery {}: {e}", delivery.bundle_id);
                let _ = requests_tx.send(ReceiveRequest::cancel()).await;
            }
        }
    }

    async fn status_notify(&self, report: BundleStatusReport) {
        let Some(kind) = Option::<StatusNotify>::from(report.assertion()) else {
            warn!("Ignoring status report with no assertion: {report:?}");
            return;
        };
        let Ok(bundle_id) = BundleId::from_key(&report.bundle_id) else {
            warn!("Ignoring status report with a malformed bundle id: {report:?}");
            return;
        };
        let Ok(from) = report.reporting_node.parse::<Eid>() else {
            warn!("Ignoring status report with a malformed reporting node: {report:?}");
            return;
        };
        // An unknown code decodes as `Unassigned`; only the reserved 255
        // fails here.
        let Ok(reason) = ReasonCode::try_from(report.reason_code) else {
            warn!("Ignoring status report with an invalid reason code: {report:?}");
            return;
        };
        let timestamp = report.status_time.and_then(from_timestamp);
        self.application
            .on_status_notify(&bundle_id, &from, kind, reason, timestamp)
            .await;
    }
}

pub struct ApplicationSession {
    ctx: Arc<ApplicationSessionCtx>,
    events: Streaming<SubscribeResponse>,
}

impl ApplicationSession {
    // Opens a session on `channel`, hands the application its sink via
    // `on_register`, and returns the EID the BPA assigned.
    pub async fn subscribe(
        channel: Channel,
        service_id: Option<Service>,
        application: Arc<dyn Application>,
        cancel: CancellationToken,
    ) -> Result<(Eid, Self)> {
        let mut client = ApplicationServiceClient::new(channel)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE);

        // Register is queued before the call opens: the server sends
        // no response headers until it reads Register.
        let (requests_tx, requests_rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
        let register = Register {
            service_id: service_id.map(|id| match id {
                Service::Ipn(n) => register::ServiceId::Ipn(n),
                Service::Dtn(name) => register::ServiceId::Dtn(name.to_string()),
            }),
        };
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(register)),
            })
            .await
            .map_err(|e| Error::Internal(e.into()))?;

        let mut events = client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .map_err(|status| match status.code() {
                // The error a local registration returns for a taken
                // service id.
                Code::AlreadyExists => Error::ServiceIdInUse(status.message().to_string()),
                _ => service_error(status),
            })?
            .into_inner();

        let Some(SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(registration)),
        }) = events.message().await.map_err(service_error)?
        else {
            return Err(Error::Internal(
                "The first event must be Registration".into(),
            ));
        };
        let eid: Eid = registration
            .endpoint_id
            .parse()
            .map_err(|e: eid::Error| Error::Internal(e.into()))?;

        let token = Token::from(registration.session_token);
        let local_unregister = Arc::new(LocalUnregister::default());
        let ctx = ApplicationSessionCtx::new(
            application.clone(),
            client.clone(),
            token.clone(),
            cancel,
            local_unregister.clone(),
        );
        application
            .on_register(
                &eid,
                Box::new(GrpcApplicationSink {
                    client,
                    token,
                    requests_tx,
                    local_unregister,
                }),
            )
            .await;
        Ok((eid, Self { ctx, events }))
    }

    // Drives the event loop until the stream ends. Deliveries run on
    // their own tasks, at most `MAX_CONCURRENT_DELIVERIES` at once;
    // `on_unregister` runs after the last of them. Returns `Ok(())`
    // when this side ended the session.
    pub async fn handle_events(self) -> Result<()> {
        let Self { ctx, mut events } = self;
        let deliveries = BoundedTaskPool::new(MAX_CONCURRENT_DELIVERIES);
        let result = loop {
            let SubscribeResponse { event } = match next_event(&mut events, &ctx.cancel).await {
                ControlFlow::Continue(response) => response,
                ControlFlow::Break(None) if ctx.local_unregister.solicited(&ctx.cancel) => {
                    break Ok(());
                }
                ControlFlow::Break(None) => break Err(Error::Disconnected),
                ControlFlow::Break(Some(status)) => break Err(session_error(status)),
            };
            let Some(event) = event else {
                warn!("Ignoring event with no payload");
                continue;
            };
            match event {
                // Registration carries the session token; never
                // Debug-format it.
                subscribe_response::Event::Registration(_) => {
                    warn!("Ignoring unexpected Registration event")
                }
                subscribe_response::Event::Delivery(delivery) => {
                    let Ok(bundle_id) = BundleId::from_key(&delivery.bundle_id) else {
                        warn!("Ignoring delivery with invalid bundle id: {delivery:?}");
                        continue;
                    };
                    let Some(expiry) = delivery.expire_time.and_then(from_timestamp) else {
                        warn!("Ignoring delivery with invalid expiry: {delivery:?}");
                        continue;
                    };
                    let ctx = ctx.clone();
                    hardy_async::spawn!(deliveries, "application_delivery", async move {
                        ctx.deliver(bundle_id, expiry, delivery).await
                    })
                    .await;
                }
                subscribe_response::Event::BundleStatusReport(report) => {
                    ctx.status_notify(report).await
                }
            }
        };
        // Drain the delivery tasks so no `on_deliver` call outlives
        // `on_unregister`.
        ctx.cancel.cancel();
        deliveries.shutdown().await;
        ctx.application.on_unregister().await;
        result
    }
}
