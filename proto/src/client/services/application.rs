// The application surface: one Subscribe session per registration, its
// events translated onto the local `Application` trait. `BpaClient`
// owns the registration itself.

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

// Dropping the sink half-closes the session stream, which the BPA
// treats as an Unregister, so `Drop` records the ending as this side's.
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
                // Refused rather than clamped: sending a different
                // lifetime would have the BPA accept a bundle nobody
                // asked for.
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

/// One live session's cross-task state: what the event loop and every
/// spawned delivery share, behind one `Arc` so each delivery task holds
/// it independently of the loop's borrows. Everything per-delivery stays
/// a parameter.
struct ApplicationSessionCtx {
    application: Arc<dyn Application>,
    client: ApplicationServiceClient<Channel>,
    token: Token,
    // A child of the client's token, so this session's end sweeps its
    // own in-flight deliveries.
    cancel: CancellationToken,
    // Tells an unregister of ours from the BPA dropping the registration.
    local_unregister: Arc<LocalUnregister>,
}

impl ApplicationSessionCtx {
    /// Builds the session's context; `cancel` is the client's token, and
    /// the context derives its own child so the session's end can sweep
    /// its in-flight deliveries without touching the client.
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

    // Collects one announced delivery into the application.
    async fn deliver(
        self: Arc<Self>,
        bundle_id: BundleId,
        expiry: OffsetDateTime,
        delivery: Delivery,
    ) {
        let mut client = self.client.clone();

        // The server parks the bundle awaiting this call, so the call is
        // opened before the application is asked for the delivery.
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
            // The ack commits the bundle. The close is awaited because
            // dropping a still-open call resets it, discarding the ack.
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
            // Cancelled in band, so the server parks the bundle for a
            // later attempt instead of holding it to expiry.
            Err(e) => {
                debug!("Application declined delivery {}: {e}", delivery.bundle_id);
                let _ = requests_tx.send(ReceiveRequest::cancel()).await;
            }
        }
    }

    // Passes one wire status report to the application.
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

/// A live session, ready to run: the event stream it drains and the
/// shared context its deliveries clone. Obtained from
/// [`subscribe`](ApplicationSession::subscribe), consumed by
/// [`handle_events`](ApplicationSession::handle_events).
pub struct ApplicationSession {
    ctx: Arc<ApplicationSessionCtx>,
    events: Streaming<SubscribeResponse>,
}

impl ApplicationSession {
    /// The Subscribe handshake plus component registration: opens the
    /// session on `channel`, hands the sink to the application via
    /// `on_register`, and returns the bound endpoint with the runnable
    /// session, which therefore cannot exist without having
    /// registered.
    pub async fn subscribe(
        channel: Channel,
        service_id: Option<Service>,
        application: Arc<dyn Application>,
        cancel: CancellationToken,
    ) -> Result<(Eid, Self)> {
        let mut client = ApplicationServiceClient::new(channel)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE);

        // The wire requires Register first, sent without waiting for
        // response headers.
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
                // The same error a local registration returns.
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

    /// The session's event loop: wire events land on the local trait;
    /// malformed events are logged and skipped. Each delivery collects on
    /// its own task, bounded by [`MAX_CONCURRENT_DELIVERIES`], so the
    /// announcement loop keeps pulling while collections run. Returns
    /// `Ok(())` when the session ends at this side's asking (the client's
    /// shutdown, or an unregister of ours round-tripping), and `Err` when
    /// it ends any other way: [`Disconnected`](services::Error::Disconnected)
    /// for a BPA that closed a session nobody here asked to end, and the
    /// stream's own error for a failure. The component's
    /// `on_unregister` runs here, after the last in-flight delivery ends:
    /// `handle_events` closes the lifecycle
    /// [`subscribe`](Self::subscribe) opened.
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
                // Not Debug-formatted: a Registration carries the session
                // token, which must never reach the logs.
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
        // No `on_deliver` call outlives `on_unregister`.
        ctx.cancel.cancel();
        deliveries.shutdown().await;
        ctx.application.on_unregister().await;
        result
    }
}
