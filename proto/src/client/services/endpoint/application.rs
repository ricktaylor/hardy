//! The application session.

use core::{ops::ControlFlow, pin::pin, time::Duration};
use std::sync::Arc;

use hardy_async::{BoundedTaskPool, CancellationToken};
use hardy_bpa::{
    async_trait,
    services::{Application, ApplicationSink, Error, Result, SendOptions},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id as BundleId,
    eid::{self, Eid, Service},
    status_report::ReasonCode,
};
use time::OffsetDateTime;
use tokio::{
    select,
    sync::mpsc::{self, Sender, WeakSender, channel},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Streaming, transport::Channel};
use tracing::{debug, warn};

use super::{registration_error, session_error, transfer_error};
use crate::{
    MAX_MESSAGE_SIZE,
    application::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, Register, SendMetadata,
        SendRequest, SubscribeRequest, SubscribeResponse,
        application_service_client::ApplicationServiceClient, receive_request, register,
        send_request, subscribe_request, subscribe_response,
    },
    chunking::{ChunkReceiver, ChunkSender, ChunkSize},
    client::services::{
        MAX_CONCURRENT_DELIVERIES, SUBSCRIBE_REQUEST_CAPACITY, TRANSFER_REQUEST_CAPACITY,
        expect_registration, next_event,
    },
    grammar::{Ack, Cancel, Unregister},
    timestamp::from_timestamp,
    token::Token,
};

/// The [`ApplicationSink`] an application receives from
/// [`ApplicationSession::subscribe`]: its `send` is a `Send` call and its
/// `unregister` an `Unregister` on the session stream.
pub struct GrpcApplicationSink {
    client: ApplicationServiceClient<Channel>,
    token: Token,
    chunk_size: ChunkSize,
    requests_tx: Sender<SubscribeRequest>,
}

#[async_trait]
impl ApplicationSink for GrpcApplicationSink {
    async fn unregister(&self) {
        let _ = self.requests_tx.send(SubscribeRequest::unregister()).await;
    }

    async fn send(
        &self,
        destination: Eid,
        lifetime: Duration,
        options: Option<SendOptions>,
        _size_hint: Option<u64>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<BundleId> {
        let metadata = SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: self.token.clone().into(),
                destination: destination.to_string(),
                lifetime: Some(prost_types::Duration {
                    seconds: i64::try_from(lifetime.as_secs())
                        .map_err(|_| Error::Internal("bundle lifetime out of range".into()))?,
                    nanos: lifetime.subsec_nanos() as i32,
                }),
                options: options.map(Into::into),
                // The hint is advisory on the trait and a declaration on the
                // wire, which the server holds the transfer to, so it is not
                // sent.
                adu_size: None,
            })),
        };
        let mut client = self.client.clone();
        let (requests_tx, requests_rx) = channel(TRANSFER_REQUEST_CAPACITY);
        let sender = ChunkSender::new(requests_tx, stream, self.chunk_size);
        // The call runs against the writing, because the server may answer
        // before the stream is finished and the rest of it then has nowhere
        // to go.
        let mut call = pin!(client.send(ReceiverStream::new(requests_rx)));
        let response = select! {
            biased;
            response = &mut call => response,
            () = sender.write_all(metadata) => call.await,
        }
        .map_err(transfer_error)?
        .into_inner();
        BundleId::from_key(&response.bundle_id).map_err(|e| Error::Internal(e.into()))
    }
}

/// The state a session's tasks share: the application, a client for its
/// data-plane calls, its token, and the token that cancels them all.
struct ApplicationSessionContext {
    application: Arc<dyn Application>,
    client: ApplicationServiceClient<Channel>,
    token: Token,
    cancel: CancellationToken,
}

impl ApplicationSessionContext {
    fn new(
        application: Arc<dyn Application>,
        client: ApplicationServiceClient<Channel>,
        token: Token,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            application,
            client,
            token,
            cancel: cancel.child_token(),
        })
    }

    /// Collects one announced delivery.
    ///
    /// Opens a `Receive` call for the bundle, hands its response stream to the
    /// application's `on_deliver` as the ADU's source, and then reports the
    /// application's verdict: an `ack` if it returned `Ok`, after which the
    /// response stream is read to its end so the ack cannot be lost in transit, or
    /// a `cancel` if it returned `Err`, so the BPA keeps the bundle. Session
    /// cancellation ends the stream the application is reading, so `on_deliver`
    /// returns on its own terms, and the BPA then keeps the bundle too.
    async fn deliver(
        self: Arc<Self>,
        bundle_id: BundleId,
        expiry: OffsetDateTime,
        delivery: Delivery,
    ) {
        let mut client = self.client.clone();

        let (requests_tx, requests_rx) = mpsc::channel::<ReceiveRequest>(TRANSFER_REQUEST_CAPACITY);
        if requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: self.token.clone().into(),
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
                    warn!("failed to collect delivery {}: {status}", delivery.bundle_id);
                    return;
                }
            },
            _ = self.cancel.cancelled() => return,
        };

        let mut receiver = ChunkReceiver::new(&mut responses, &self.cancel);
        let result = self
            .application
            .on_deliver(
                &bundle_id,
                expiry,
                delivery.ack_requested,
                delivery.adu_size,
                &mut receiver,
            )
            .await;

        match result {
            Ok(()) => {
                tokio::select! {
                    biased;
                    _ = async {
                        if requests_tx.send(ReceiveRequest::ack()).await.is_ok() {
                            match responses.message().await {
                                Ok(None) => {}
                                Err(status) => debug!("collection stream failed: {status}"),
                                Ok(Some(_)) => debug!("ignoring unexpected message after ack"),
                            }
                        }
                    } => {}
                    _ = self.cancel.cancelled() => {}
                }
            }
            Err(e) => {
                debug!("application declined delivery {}: {e}", delivery.bundle_id);
                let _ = requests_tx.send(ReceiveRequest::cancel()).await;
            }
        }
    }

    /// Passes a bundle status report to the application.
    ///
    /// A report with no assertion, a malformed bundle id or reporting node, or a
    /// reserved reason code is logged and dropped; a status time that cannot be
    /// represented is passed as `None`.
    async fn status_notify(&self, report: BundleStatusReport) {
        let Some(kind) = report.assertion().notify() else {
            warn!("ignoring status report with no assertion: {report:?}");
            return;
        };
        let Ok(bundle_id) = BundleId::from_key(&report.bundle_id) else {
            warn!("ignoring status report with a malformed bundle id: {report:?}");
            return;
        };
        let Ok(from) = report.reporting_node.parse::<Eid>() else {
            warn!("ignoring status report with a malformed reporting node: {report:?}");
            return;
        };
        let Ok(reason) = ReasonCode::try_from(report.reason_code) else {
            warn!("ignoring status report with an invalid reason code: {report:?}");
            return;
        };
        let timestamp = report.status_time.and_then(from_timestamp);
        self.application
            .on_status_notify(&bundle_id, &from, kind, reason, timestamp)
            .await;
    }
}

/// A registered application's session, from the registration handshake to the
/// end of its event stream.
pub struct ApplicationSession {
    ctx: Arc<ApplicationSessionContext>,
    requests_tx: WeakSender<SubscribeRequest>,
    events: Streaming<SubscribeResponse>,
}

impl ApplicationSession {
    /// Opens the session and registers the application.
    ///
    /// Sends `Register` for `service_id`, or for a service number of the BPA's
    /// choosing if `None`, waits for the `Registration` event, and runs the
    /// application's `on_register` with its sink before returning the assigned
    /// endpoint id and the session, which
    /// [`handle_events`](ApplicationSession::handle_events) then drives. `cancel`
    /// is the token that ends the session from this side.
    ///
    /// # Errors
    ///
    /// Returns the BPA's error as recorded on the status, `ServiceIdInUse` for a
    /// bare `ALREADY_EXISTS`, `Disconnected` if the BPA is unreachable or `cancel`
    /// fires during the handshake, and `Internal` for any other status or for an
    /// endpoint id that does not parse.
    pub async fn subscribe(
        channel: Channel,
        service_id: Option<Service>,
        application: Arc<dyn Application>,
        cancel: CancellationToken,
    ) -> Result<(Eid, Self)> {
        let mut client = ApplicationServiceClient::new(channel)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE);

        let (requests_tx, requests_rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
        let weak_requests_tx = requests_tx.downgrade();
        let register = Register {
            service_id: service_id.as_ref().map(|id| match id {
                Service::Ipn(n) => register::ServiceId::Ipn(*n),
                Service::Dtn(name) => register::ServiceId::Dtn(name.to_string()),
            }),
            // The SDK buffers at `DEFAULT_CHUNK_SIZE`, so it asks for no smaller ceiling.
            max_chunk_size: None,
        };
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(register)),
            })
            .await
            .map_err(|e| Error::Internal(e.into()))?;

        let (events, registration) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Disconnected),
            handshake = async {
                let mut events = client
                    .subscribe(ReceiverStream::new(requests_rx))
                    .await
                    .map_err(|s| registration_error(s, service_id.as_ref()))?
                    .into_inner();
                let registration = expect_registration(&mut events)
                    .await
                    .map_err(|s| registration_error(s, service_id.as_ref()))?;
                Ok::<_, Error>((events, registration))
            } => handshake?,
        };
        let eid: Eid = registration
            .endpoint_id
            .parse()
            .map_err(|e: eid::Error| Error::Internal(e.into()))?;

        // A server announcing a size no session may run at is not one this
        // crate wrote, so the SDK runs at its own default instead.
        let chunk_size = registration
            .sizes
            .as_ref()
            .and_then(|sizes| ChunkSize::new(sizes.chunk_size))
            .unwrap_or_default();
        let token = Token::from(registration.session_token);
        let ctx = ApplicationSessionContext::new(
            application.clone(),
            client.clone(),
            token.clone(),
            cancel,
        );
        application
            .on_register(
                &eid,
                Box::new(GrpcApplicationSink {
                    client,
                    token,
                    chunk_size,
                    requests_tx,
                }),
            )
            .await;
        Ok((
            eid,
            Self {
                ctx,
                requests_tx: weak_requests_tx,
                events,
            },
        ))
    }

    /// Runs the session until its stream ends, fails, or is cancelled.
    ///
    /// Deliveries are collected concurrently on a pool of
    /// [`MAX_CONCURRENT_DELIVERIES`] tasks, so an announcement waits while the pool
    /// is full; status reports are passed on inline. On the way out collections
    /// still in flight are cancelled and awaited, and the application's
    /// `on_unregister` runs, and the session sends `Unregister`, so the BPA retires
    /// the token even when it was this side that ended the session.
    ///
    /// # Errors
    ///
    /// Returns the error the stream failed with, mapped as described on
    /// [`RegistrationHandle`](crate::client::RegistrationHandle).
    pub async fn handle_events(self) -> Result<()> {
        let Self {
            ctx,
            requests_tx: weak_requests_tx,
            mut events,
        } = self;
        let deliveries = BoundedTaskPool::new(MAX_CONCURRENT_DELIVERIES);
        let result = loop {
            let SubscribeResponse { event, .. } = match next_event(&mut events, &ctx.cancel).await {
                ControlFlow::Continue(response) => response,
                ControlFlow::Break(None) => break Ok(()),
                ControlFlow::Break(Some(status)) => break Err(session_error(status)),
            };
            let Some(event) = event else {
                warn!("ignoring event with no payload");
                continue;
            };
            match event {
                subscribe_response::Event::Registration(_) => {
                    warn!("ignoring unexpected Registration event")
                }
                subscribe_response::Event::Delivery(delivery) => {
                    let Ok(bundle_id) = BundleId::from_key(&delivery.bundle_id) else {
                        warn!("ignoring delivery with invalid bundle id: {delivery:?}");
                        continue;
                    };
                    let Some(expiry) = delivery.expire_time.and_then(from_timestamp) else {
                        warn!("ignoring delivery with invalid expiry: {delivery:?}");
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
        ctx.cancel.cancel();
        deliveries.shutdown().await;
        ctx.application.on_unregister().await;

        // End the registration from this side, so the BPA retires the token
        // now rather than when the connection dies. The sender is weak: the
        // component's sink owns the request stream, and dropping that sink is
        // its own way of ending the registration.
        if let Some(requests_tx) = weak_requests_tx.upgrade() {
            let _ = requests_tx.try_send(SubscribeRequest::unregister());
        }
        result
    }
}
