// The `hardy.application.v1` service. `Subscribe` opens a
// registration session, `Send` streams an outbound ADU into the BPA,
// and deliveries are announced on the session stream and collected by
// a `Receive` call.

use core::time::Duration;
use std::sync::Arc;

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::TaskPool;
use hardy_bpa::{
    Bytes, async_trait,
    bpa::BpaRegistration,
    services::{self, ApplicationSink, SendOptions},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id as BundleId,
    eid::{Eid, Service},
    status_report,
};
use time::OffsetDateTime;
#[cfg(test)]
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::{Instrument, Span, instrument, trace_span};
use tracing::{debug, warn};

use super::{service_status, session_sub};
use crate::{
    MAX_TRANSFER_SIZE,
    application::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse,
        Registration, SendMetadata, SendRequest, SendResponse, StatusAssertion, SubscribeRequest,
        SubscribeResponse, application_service_server::ApplicationService, receive_request,
        register, send_request, subscribe_request, subscribe_response,
    },
    grammar::{Ack, Cancel},
    server::{
        Limits,
        adapter::{RequestReader, ResponseWriter},
        announce::{Announcements, Collection},
        error::{self, Error},
        leases::Lease,
        session::{Session, SessionStream},
    },
    timestamp::to_timestamp,
    token::Token,
    transfer::Writer,
};

// The surface name used in spans, lease warnings, and the token `sub`.
const LABEL: &str = "application";

// A subscription's event buffer, in messages.
const EVENT_DEPTH: usize = 16;

// `MAX_TRANSFER_SIZE` clamped to the host's addressable range, so a
// declared ADU size beyond it fails as a status rather than a 32-bit
// allocation panic.
const MAX_ADU_SIZE: u64 = if MAX_TRANSFER_SIZE > isize::MAX as u64 {
    isize::MAX as u64
} else {
    MAX_TRANSFER_SIZE
};

// The per-session `services::Application` registered with the BPA.
struct GrpcApplication {
    session: Session<SubscribeResponse, Arc<dyn ApplicationSink>>,
    deliveries: Announcements<ReceiveResponse, ReceiveRequest>,
}

impl GrpcApplication {
    fn new(session: Session<SubscribeResponse, Arc<dyn ApplicationSink>>) -> Self {
        Self {
            deliveries: Announcements::new(session.leases().clone()),
            session,
        }
    }

    async fn event(&self, event: subscribe_response::Event) -> services::Result<()> {
        self.session
            .event(SubscribeResponse { event: Some(event) })
            .await
            .map_err(services::Error::from)
    }

    // Waits for the Ack or Cancel that settles the delivery, ignoring
    // other messages. Cancel-safe: the `select!` callers drop and
    // re-create it.
    async fn acked(requests: &mut Streaming<ReceiveRequest>) -> error::Result<()> {
        let mut warned = false;
        loop {
            break match requests.message().await {
                Ok(Some(request)) if request.is_ack() => Ok(()),
                Ok(Some(request)) if request.is_cancel() => Err(Error::Cancelled),
                // The message may carry the session token; never
                // Debug-format it.
                Ok(Some(_)) => {
                    if !warned {
                        warned = true;
                        warn!("Ignoring unexpected message on the Receive request side");
                    }
                    continue;
                }
                Ok(None) => Err(Error::RequestStreamClosed),
                Err(e) => {
                    debug!("Receive stream failed: {e}");
                    Err(Error::RequestStreamFailed)
                }
            };
        }
    }
}

#[async_trait]
impl services::Application for GrpcApplication {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        self.session.register(Arc::from(sink));
    }

    async fn on_unregister(&self) {
        self.session.abort();
    }

    async fn on_deliver(
        &self,
        bundle_id: &BundleId,
        expiry: OffsetDateTime,
        ack_requested: bool,
        adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let announced = self.deliveries.announce(bundle_id).map_err(|e| {
            warn!("Refusing a second announcement for a bundle already being delivered");
            services::Error::from(e)
        })?;

        self.event(subscribe_response::Event::Delivery(Delivery {
            bundle_id: bundle_id.to_key(),
            source: bundle_id.source.to_string(),
            expire_time: Some(to_timestamp(expiry)),
            ack_requested,
            adu_size,
        }))
        .await?;

        let Collection {
            responses_tx,
            mut requests,
        } = announced.collected().await?;
        let leases = self.session.leases();

        let writer = ResponseWriter::new(&responses_tx, leases, stream);

        tokio::select! {
            biased;
            acked = Self::acked(&mut requests) => {
                let e = match acked {
                    Ok(()) => Error::ProtocolViolation,
                    Err(e) => e,
                };
                if let Some(status) = e.status() {
                    let _ = responses_tx.try_send(Err(status));
                }
                return Err(e.into());
            }
            result = writer.write_all() => match result {
                Ok(()) => {}
                Err(e) => return Err(e.into()),
            },
        }

        // Chunks sent, wait for the Ack or Cancel.
        tokio::select! {
            biased;
            _ = leases.cancelled() => {
                if let Some(status) = Error::SessionClosed.status() {
                    let _ = responses_tx.try_send(Err(status));
                }
                Err(Error::SessionClosed.into())
            }
            acked = Self::acked(&mut requests) => match acked {
                Ok(()) => Ok(()),
                Err(e) => {
                    if let Some(status) = e.status() {
                        let _ = responses_tx.try_send(Err(status));
                    }
                    Err(e.into())
                }
            },
            expired = leases.expired(Lease::Ack) => {
                if let Some(status) = expired.status() {
                    let _ = responses_tx.try_send(Err(status));
                }
                Err(expired.into())
            }
        }
    }

    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        from: &Eid,
        kind: services::StatusNotify,
        reason: status_report::ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        let _ = self
            .event(subscribe_response::Event::BundleStatusReport(
                BundleStatusReport {
                    bundle_id: bundle_id.to_key(),
                    reporting_node: from.to_string(),
                    assertion: StatusAssertion::from(kind).into(),
                    reason_code: u64::from(reason),
                    status_time: timestamp.map(to_timestamp),
                },
            ))
            .await;
    }
}

/// The server implementation of the `hardy.application.v1` service.
///
/// Clones share one set of sessions. Shut down the pool given to
/// [`new`](Self::new) only after the transport has stopped accepting:
/// pool shutdown tears down every subscription.
#[derive(Clone)]
pub struct ApplicationServiceImpl {
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    limits: Limits,
    sessions: Arc<DashMap<Token, Arc<GrpcApplication>, RandomState>>,
    #[cfg(test)]
    hooks: super::tests::Hooks,
}

impl ApplicationServiceImpl {
    /// Creates the service with default [`Limits`].
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self::with_limits(bpa, tasks, Limits::default())
    }

    /// Creates the service with the given [`Limits`].
    pub fn with_limits(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool, limits: Limits) -> Self {
        Self {
            bpa,
            tasks,
            limits,
            sessions: Arc::new(DashMap::with_hasher(RandomState::default())),
            #[cfg(test)]
            hooks: super::tests::Hooks::default(),
        }
    }

    async fn subscription(
        self,
        mut requests: Streaming<SubscribeRequest>,
        response_tx: oneshot::Sender<Result<Response<SessionStream<SubscribeResponse>>, Status>>,
    ) {
        // Only the pool can cancel this wait: no session exists yet.
        #[cfg(test)]
        let _ = self.hooks.opened.send(());
        let first = tokio::select! {
            biased;
            _ = self.tasks.cancel_token().cancelled() => {
                let _ = response_tx.send(Err(Status::unavailable("Shutting down")));
                return;
            }
            message = requests.message() => message,
        };
        let register = match first {
            Ok(Some(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(register)),
            })) => register,
            Ok(_) => {
                let _ = response_tx.send(Err(Status::invalid_argument(
                    "The first message must be Register",
                )));
                return;
            }
            Err(e) => {
                let _ = response_tx.send(Err(e));
                return;
            }
        };

        // `None` asks for a dynamic registration.
        let service = register.service_id.map(|id| match id {
            register::ServiceId::Ipn(n) => Service::Ipn(n),
            register::ServiceId::Dtn(demux) => Service::Dtn(demux.into()),
        });

        // Created before the BPA registration: events raised during
        // registration buffer in the session.
        let token = Token::mint(&session_sub(LABEL, service.as_ref()));
        let session = Session::new(self.tasks.child_token(), LABEL, self.limits, EVENT_DEPTH);
        let application = Arc::new(GrpcApplication::new(session));

        let endpoint_id = match self.register(service, application.clone()).await {
            Ok(endpoint_id) => endpoint_id,
            Err(e) => {
                let _ = response_tx.send(Err(service_status(e)));
                return;
            }
        };

        self.sessions.insert(token.clone(), application.clone());

        let registration = SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(Registration {
                endpoint_id: endpoint_id.to_string(),
                session_token: token.clone().into(),
            })),
        };
        let mut stream = application.session.open(registration);
        let _guard = stream.cancel_guard();

        if response_tx.send(Ok(Response::new(stream))).is_ok() {
            application.session.serve(requests).await;
        }

        // In order: stop the work, drop the token, unregister; the
        // stream ends last.
        application.session.abort();
        self.sessions.remove(&token);
        if let Some(sink) = application.session.registered() {
            sink.unregister().await;
        }
        #[cfg(test)]
        let _ = self.hooks.torn_down.send(token);
    }

    async fn register(
        &self,
        service: Option<Service>,
        application: Arc<GrpcApplication>,
    ) -> services::Result<Eid> {
        match service {
            Some(service) => self.bpa.register_application(service, application).await,
            None => self.bpa.register_dynamic_application(application).await,
        }
    }

    fn resolve(&self, token: Bytes) -> Result<Arc<GrpcApplication>, Status> {
        self.sessions
            .get(&Token::from(token))
            .map(|application| application.clone())
            .ok_or_else(|| Status::unauthenticated("Unknown session token"))
    }
}

#[async_trait]
impl ApplicationService for ApplicationServiceImpl {
    type SubscribeStream = SessionStream<SubscribeResponse>;

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn subscribe(
        &self,
        request: Request<Streaming<SubscribeRequest>>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let (response_tx, response_rx) = oneshot::channel();

        let subscription = self.clone().subscription(request.into_inner(), response_tx);
        #[cfg(feature = "instrument")]
        {
            let span = trace_span!(parent: None, "grpc_session", surface = LABEL);
            span.follows_from(Span::current());
            self.tasks.spawn(subscription.instrument(span));
        }
        #[cfg(not(feature = "instrument"))]
        self.tasks.spawn(subscription);

        response_rx
            .await
            .unwrap_or_else(|_| Err(Status::unavailable("Shutting down")))
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn send(
        &self,
        request: Request<Streaming<SendRequest>>,
    ) -> Result<Response<SendResponse>, Status> {
        let mut requests = request.into_inner();

        let Some(send_request::Request::Metadata(SendMetadata {
            session_token,
            destination,
            lifetime,
            options,
            adu_size,
        })) = requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be the metadata",
            ));
        };
        let application = self.resolve(session_token)?;

        let destination = destination
            .parse::<Eid>()
            .map_err(|e| Status::invalid_argument(format!("Invalid destination: {e}")))?;
        let lifetime = lifetime
            .ok_or_else(|| Status::invalid_argument("Missing lifetime"))
            .and_then(|d| {
                Duration::try_from(d)
                    .map_err(|e| Status::invalid_argument(format!("Invalid lifetime: {e}")))
            })?;

        if adu_size.is_some_and(|size| size > MAX_ADU_SIZE) {
            return Err(Status::resource_exhausted(
                "Declared ADU size exceeds the maximum transfer size",
            ));
        }

        let options = options.map(SendOptions::from);

        let mut reader = RequestReader::new(requests, application.session.leases().clone(), "Send");
        match application
            .session
            .registered()
            .ok_or_else(|| service_status(services::Error::Disconnected))?
            .send(destination, lifetime, options, adu_size, &mut reader)
            .await
        {
            Ok(bundle_id) => Ok(Response::new(SendResponse {
                bundle_id: bundle_id.to_key(),
            })),
            Err(e) => Err(reader.status().unwrap_or_else(|| service_status(e))),
        }
    }

    type ReceiveStream = ReceiverStream<Result<ReceiveResponse, Status>>;

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn receive(
        &self,
        request: Request<Streaming<ReceiveRequest>>,
    ) -> Result<Response<Self::ReceiveStream>, Status> {
        let mut requests = request.into_inner();

        let Some(receive_request::Request::Metadata(ReceiveMetadata {
            session_token,
            bundle_id,
        })) = requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be the metadata",
            ));
        };
        let application = self.resolve(session_token)?;
        let Some(responses_rx) = application.deliveries.collect(&bundle_id, requests) else {
            return Err(Status::not_found("No such delivery"));
        };
        Ok(Response::new(responses_rx))
    }
}

// Wire tests against a real BPA.
#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    #[cfg(feature = "client")]
    use crate::client::BpaClient;
    #[cfg(feature = "client")]
    use hardy_async::sync::spin::Once;
    #[cfg(feature = "client")]
    use hardy_bpa::stream::concat_stream;
    use hardy_bpa::{
        bpa::Bpa,
        cla::{self, Cla, ClaInit},
        node_ids::NodeIds,
        policy::FlowControllerFactory,
        routing::{self, RoutingAgent},
        // The bare `Service` is the eid type (via `super::*`), so the
        // BPA service trait takes the alias.
        services::Service as BpaService,
    };
    use hardy_bpv7::eid::{DtnNodeId, IpnNodeId, NodeId};
    use tokio::sync::Notify;
    use tonic::{
        Code,
        transport::{Channel, Server},
    };

    use super::{
        super::tests::{build_bpa, ipn1, serve, timeout, wait_torn_down},
        *,
    };
    use crate::application::{
        Register, Unregister, application_service_client::ApplicationServiceClient,
        application_service_server::ApplicationServiceServer, receive_response,
    };
    #[cfg(feature = "client")]
    use crate::client::MAX_CONCURRENT_DELIVERIES;
    use crate::server::announce::DATA_CHANNEL_DEPTH;

    struct Harness {
        bpa: Arc<Bpa>,
        // Held live: dropping the pool would tear down the sessions.
        tasks: TaskPool,
        client: ApplicationServiceClient<Channel>,
        #[cfg_attr(
            not(feature = "client"),
            expect(dead_code, reason = "read by the client SDK test")
        )]
        address: SocketAddr,
        // A second handle on the surface, for its test hooks.
        surface: ApplicationServiceImpl,
    }

    // A running BPA (node ipn:1) behind the surface on a port-0
    // listener, plus a connected generated client.
    async fn harness() -> Harness {
        harness_with(ipn1()).await
    }

    async fn harness_with(node_ids: NodeIds) -> Harness {
        harness_with_limits(node_ids, Limits::default()).await
    }

    async fn harness_with_limits(node_ids: NodeIds, limits: Limits) -> Harness {
        // Status reports on, for the report round-trip test.
        let bpa = build_bpa(node_ids, true).await;

        let tasks = TaskPool::new();
        let surface = ApplicationServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits);
        let service = ApplicationServiceServer::new(surface.clone());
        let address = serve(Server::builder().add_service(service)).await;

        let client = ApplicationServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        Harness {
            bpa,
            tasks,
            client,
            address,
            surface,
        }
    }

    struct App {
        requests_tx: mpsc::Sender<SubscribeRequest>,
        events: Streaming<SubscribeResponse>,
        endpoint_id: String,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(
        client: &mut ApplicationServiceClient<Channel>,
        service_id: Option<register::ServiceId>,
    ) -> App {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id,
                })),
            })
            .await
            .unwrap();

        let mut events = client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();
        let event = timeout(events.message()).await.unwrap().unwrap();
        let Some(subscribe_response::Event::Registration(registration)) = event.event else {
            panic!("expected the Registration event first, got {event:?}");
        };
        assert!(!registration.session_token.is_empty());

        App {
            requests_tx,
            events,
            endpoint_id: registration.endpoint_id,
            token: registration.session_token,
        }
    }

    async fn send(
        client: &mut ApplicationServiceClient<Channel>,
        token: Bytes,
        destination: &str,
        adu: &[u8],
    ) -> Result<SendResponse, Status> {
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: token,
                    destination: destination.to_string(),
                    lifetime: Some(prost_types::Duration {
                        seconds: 3600,
                        nanos: 0,
                    }),
                    options: None,
                    adu_size: None,
                })),
            },
            SendRequest {
                request: Some(send_request::Request::LastChunk(Bytes::copy_from_slice(
                    adu,
                ))),
            },
        ];
        client
            .send(tokio_stream::iter(messages))
            .await
            .map(|response| response.into_inner())
    }

    // Collects one announced delivery, committing it with the in-band
    // Ack; `abandon` sends Cancel after the first chunk instead.
    async fn collect(
        client: &mut ApplicationServiceClient<Channel>,
        token: Bytes,
        bundle_id: &str,
        abandon: bool,
    ) -> Result<Vec<u8>, Status> {
        // The request side stays open for the whole collection:
        // metadata first, then Ack or Cancel later.
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(4);
        requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: token,
                    bundle_id: bundle_id.to_string(),
                })),
            })
            .await
            .unwrap();

        let mut stream = client
            .receive(tokio_stream::wrappers::ReceiverStream::new(requests_rx))
            .await?
            .into_inner();
        let mut collected = Vec::new();
        let mut cancelled = false;
        loop {
            match stream.message().await?.and_then(|r| r.response) {
                Some(receive_response::Response::Chunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    if abandon && !cancelled {
                        cancelled = true;
                        let _ = requests_tx
                            .send(ReceiveRequest {
                                request: Some(receive_request::Request::Cancel(())),
                            })
                            .await;
                    }
                }
                Some(receive_response::Response::LastChunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    if abandon {
                        // The last chunk may already be queued behind
                        // the Cancel; keep reading until the server
                        // closes.
                        continue;
                    }
                    let _ = requests_tx
                        .send(ReceiveRequest {
                            request: Some(receive_request::Request::Ack(())),
                        })
                        .await;
                    // Drain to end-of-stream so the Ack reaches the
                    // server before the call is dropped.
                    while stream.message().await?.is_some() {}
                    return Ok(collected);
                }
                // The server closed after the Cancel; return what
                // arrived.
                None if abandon => return Ok(collected),
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
    }

    // Waits for the Delivery announcing `adu_size` bytes on the
    // session stream.
    async fn delivery(app: &mut App, adu_size: u64) -> Delivery {
        loop {
            let event = timeout(app.events.message()).await.unwrap().unwrap();
            match event.event {
                Some(subscribe_response::Event::Delivery(delivery)) => {
                    assert_eq!(delivery.adu_size, adu_size);
                    return delivery;
                }
                // Status reports about the sent bundle may interleave.
                Some(subscribe_response::Event::BundleStatusReport(_)) => {}
                other => panic!("expected a Delivery, got {other:?}"),
            }
        }
    }

    // Sends `adu` as a chunked transfer, for the tests that need
    // several wire chunks in flight.
    async fn send_chunked(
        client: &mut ApplicationServiceClient<Channel>,
        token: Bytes,
        destination: String,
        adu: &[u8],
    ) {
        let mut messages = vec![SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: token,
                destination,
                lifetime: Some(prost_types::Duration {
                    seconds: 3600,
                    nanos: 0,
                }),
                options: None,
                adu_size: None,
            })),
        }];
        for chunk in adu.chunks(crate::CHUNK_SIZE) {
            messages.push(SendRequest {
                request: Some(send_request::Request::Chunk(Bytes::copy_from_slice(chunk))),
            });
        }
        messages.push(SendRequest {
            request: Some(send_request::Request::LastChunk(Bytes::new())),
        });
        client.send(tokio_stream::iter(messages)).await.unwrap();
    }

    // Takes a Receive through its last chunk without acknowledging,
    // leaving the request side open for the caller's verdict.
    async fn take_all(
        client: &mut ApplicationServiceClient<Channel>,
        token: Bytes,
        bundle_id: &str,
    ) -> (
        mpsc::Sender<ReceiveRequest>,
        Streaming<ReceiveResponse>,
        Vec<u8>,
    ) {
        let (requests_tx, requests_rx) = mpsc::channel(2);
        requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: token,
                    bundle_id: bundle_id.to_string(),
                })),
            })
            .await
            .unwrap();
        let mut stream = client
            .receive(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();
        let mut collected = Vec::new();
        loop {
            match timeout(stream.message())
                .await
                .unwrap()
                .unwrap()
                .response
                .unwrap()
            {
                receive_response::Response::Chunk(chunk) => collected.extend_from_slice(&chunk),
                receive_response::Response::LastChunk(chunk) => {
                    collected.extend_from_slice(&chunk);
                    break;
                }
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
        (requests_tx, stream, collected)
    }

    // Reads the response stream to its terminal status.
    async fn terminal_status(stream: &mut Streaming<ReceiveResponse>) -> Status {
        loop {
            match timeout(stream.message()).await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("an abandonment must end with a status"),
                Err(status) => break status,
            }
        }
    }

    // Reads the response stream to a clean close.
    async fn clean_end(stream: &mut Streaming<ReceiveResponse>) {
        loop {
            match timeout(stream.message()).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(status) => panic!("a cancel must end the call cleanly, got {status:?}"),
            }
        }
    }

    // Shared tail of the parking tests: ends the session, re-registers,
    // and collects the parked bundle whole.
    async fn recollected_after_reregistration(harness: &mut Harness, app: App, adu: &[u8]) {
        let App {
            requests_tx,
            mut events,
            ..
        } = app;
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(timeout(events.message()).await.unwrap().is_none());

        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let announced = delivery(&mut app, adu.len() as u64).await;
        let collected = collect(
            &mut harness.client,
            app.token.clone(),
            &announced.bundle_id,
            false,
        )
        .await
        .unwrap();
        assert_eq!(collected, adu);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_and_dynamic_registrations_mint_distinct_sessions() {
        let mut harness = harness().await;

        let explicit = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        assert_eq!(explicit.endpoint_id, "ipn:1.7");

        let dynamic = register(&mut harness.client, None).await;
        assert!(!dynamic.endpoint_id.is_empty());
        assert_ne!(dynamic.endpoint_id, explicit.endpoint_id);
        assert_ne!(dynamic.token, explicit.token);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_to_self_roundtrip() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = b"hello over the v1 wire";
        let destination = app.endpoint_id.clone();
        let sent = send(&mut harness.client, app.token.clone(), &destination, adu)
            .await
            .unwrap();
        assert!(!sent.bundle_id.is_empty());

        let delivery = delivery(&mut app, adu.len() as u64).await;
        assert_eq!(delivery.source, app.endpoint_id);

        let collected = collect(
            &mut harness.client,
            app.token.clone(),
            &delivery.bundle_id,
            false,
        )
        .await
        .unwrap();
        assert_eq!(collected, adu);

        // The sent and announced ids match, and the completed
        // collection consumed the delivery.
        assert_eq!(delivery.bundle_id, sent.bundle_id);
        let gone = collect(
            &mut harness.client,
            app.token.clone(),
            &delivery.bundle_id,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(gone.code(), Code::NotFound);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_truncated_send_never_commits() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // A Chunk but no LastChunk: the half-close is a truncation.
        let destination = app.endpoint_id.clone();
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: app.token.clone(),
                    destination,
                    lifetime: Some(prost_types::Duration {
                        seconds: 3600,
                        nanos: 0,
                    }),
                    options: None,
                    adu_size: None,
                })),
            },
            SendRequest {
                request: Some(send_request::Request::Chunk(Bytes::from_static(b"partial"))),
            },
        ];
        let status = harness
            .client
            .send(tokio_stream::iter(messages))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Aborted);

        // `shutdown()` joins the BPA workers, so anything to announce
        // has been announced and the session stream then ends; drain
        // it and assert no Delivery arrived.
        harness.bpa.shutdown().await;
        while let Some(event) = app.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "a truncated send must not deliver"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_send_is_discarded() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let destination = app.endpoint_id.clone();
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: app.token.clone(),
                    destination,
                    lifetime: Some(prost_types::Duration {
                        seconds: 3600,
                        nanos: 0,
                    }),
                    options: None,
                    adu_size: None,
                })),
            },
            SendRequest {
                request: Some(send_request::Request::Chunk(Bytes::from_static(b"undo"))),
            },
            SendRequest {
                request: Some(send_request::Request::Cancel(())),
            },
        ];
        let status = harness
            .client
            .send(tokio_stream::iter(messages))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Cancelled);

        // `shutdown()` joins the BPA workers, so anything to announce
        // has been announced and the session stream then ends; drain
        // it and assert no Delivery arrived.
        harness.bpa.shutdown().await;
        while let Some(event) = app.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "a cancelled send must not deliver"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receive_of_an_unannounced_id_is_not_found() {
        let mut harness = harness().await;
        let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Never announced (malformed ids included): nothing is held
        // under that key for this session.
        let status = collect(&mut harness.client, app.token.clone(), "not a key", false)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::NotFound);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_collection_defers_to_the_next_registration() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = vec![0x5a; crate::CHUNK_SIZE + 3];
        let destination = app.endpoint_id.clone();
        send(&mut harness.client, app.token.clone(), &destination, &adu)
            .await
            .unwrap();
        let first = delivery(&mut app, adu.len() as u64).await;

        // Abandoning with an in-band cancel stops the collection
        // without acknowledging it: the client asked for the ending, so
        // the call closes cleanly, and the bundle is parked, not
        // finalised.
        collect(
            &mut harness.client,
            app.token.clone(),
            &first.bundle_id,
            true,
        )
        .await
        .expect("a cancelled collection ends cleanly");

        // The announced stream was the single collection capability:
        // a repeat collection in this session answers not-found.
        let spent = collect(
            &mut harness.client,
            app.token.clone(),
            &first.bundle_id,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(spent.code(), Code::NotFound);

        recollected_after_reregistration(&mut harness, app, &adu).await;

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forged_token_is_rejected() {
        let mut harness = harness().await;
        register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let status = send(
            &mut harness.client,
            Bytes::from_static(b"forged"),
            "ipn:1.7",
            b"denied",
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_stream_tears_the_session_down() {
        let mut harness = harness().await;
        let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // The client vanishes without Unregister. Waiting on the
        // teardown signal makes the rejection below race-free.
        let mut torn = harness.surface.hooks.torn_down.subscribe();
        drop(app.events);
        drop(app.requests_tx);
        wait_torn_down(&mut torn, &app.token).await;

        let status = send(&mut harness.client, app.token.clone(), "ipn:1.7", b"stale")
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_shutdown_tears_sessions_and_drains() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Every session token is a child of the pool's, so shutdown
        // must end the stream with no client action.
        let shutdown = tokio::spawn({
            let tasks = harness.tasks.clone();
            async move { tasks.shutdown().await }
        });
        assert!(
            timeout(app.events.message()).await.unwrap().is_none(),
            "pool shutdown must end the session stream"
        );
        timeout(shutdown).await.unwrap();

        harness.bpa.shutdown().await;
    }

    // An empty ADU delivers end to end: a lone empty last chunk counts
    // as a completion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_adu_delivers_end_to_end() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let destination = app.endpoint_id.clone();
        send(&mut harness.client, app.token.clone(), &destination, b"")
            .await
            .unwrap();

        let announced = delivery(&mut app, 0).await;
        let collected = collect(
            &mut harness.client,
            app.token.clone(),
            &announced.bundle_id,
            false,
        )
        .await
        .unwrap();
        assert!(collected.is_empty());

        // The empty collection still completed and consumed the
        // delivery.
        let gone = collect(
            &mut harness.client,
            app.token.clone(),
            &announced.bundle_id,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(gone.code(), Code::NotFound);

        harness.bpa.shutdown().await;
    }

    // Above the bound is rejected before any bytes arrive; within it
    // the declaration is a hint, and an inaccurate one still commits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_declared_adu_size_above_the_bound_is_rejected_preflight() {
        let mut harness = harness().await;
        let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let metadata = |adu_size| SendMetadata {
            session_token: app.token.clone(),
            destination: app.endpoint_id.clone(),
            lifetime: Some(prost_types::Duration {
                seconds: 3600,
                nanos: 0,
            }),
            options: None,
            adu_size,
        };

        let messages = [SendRequest {
            request: Some(send_request::Request::Metadata(metadata(Some(
                MAX_ADU_SIZE + 1,
            )))),
        }];
        let status = harness
            .client
            .send(tokio_stream::iter(messages))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);

        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(metadata(Some(1024 * 1024)))),
            },
            SendRequest {
                request: Some(send_request::Request::LastChunk(Bytes::from_static(
                    b"smaller than declared",
                ))),
            },
        ];
        let sent = harness
            .client
            .send(tokio_stream::iter(messages))
            .await
            .unwrap()
            .into_inner();
        assert!(!sent.bundle_id.is_empty());

        harness.bpa.shutdown().await;
    }

    // The announcement is recorded before the Delivery event goes out,
    // so an early NOT_FOUND neither consumes nor poisons anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_receive_racing_the_announcement_lands() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = b"raced";
        let destination = app.endpoint_id.clone();
        let sent = send(&mut harness.client, app.token.clone(), &destination, adu)
            .await
            .unwrap();

        // Polled without reading the session stream: early attempts may
        // answer NOT_FOUND, and the first success collects the whole ADU.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let collected = loop {
            match collect(
                &mut harness.client,
                app.token.clone(),
                &sent.bundle_id,
                false,
            )
            .await
            {
                Ok(collected) => break collected,
                Err(status) => {
                    assert_eq!(status.code(), Code::NotFound);
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the announcement never landed"
                    );
                    tokio::task::yield_now().await;
                }
            }
        };
        assert_eq!(collected, adu);

        // The Delivery event still arrives on the session stream even
        // though the collection already completed.
        let announced = delivery(&mut app, adu.len() as u64).await;
        assert_eq!(announced.bundle_id, sent.bundle_id);

        harness.bpa.shutdown().await;
    }

    // A session dying with a claimed Receive mid-stream leaves the
    // bundle parked for the next registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_death_mid_receive_defers_the_delivery() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Far more than the server's buffer plus transport windows can
        // absorb, so a client that does not read parks the writer well
        // before the final segment.
        let adu = vec![0x5a; 16 * crate::CHUNK_SIZE];
        let destination = app.endpoint_id.clone();
        send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
        let announced = delivery(&mut app, adu.len() as u64).await;

        // Awaiting the call means the handler ran and the stream is
        // claimed. Then read nothing.
        let (requests_tx, requests_rx) = mpsc::channel(2);
        requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: app.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        let mut claimed = harness
            .client
            .receive(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();

        // Kill the session with the collection mid-stream.
        app.requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(timeout(app.events.message()).await.unwrap().is_none());

        // The claimed stream must end without its last chunk.
        loop {
            match claimed.message().await {
                Ok(Some(ReceiveResponse {
                    response: Some(receive_response::Response::Chunk(_)),
                })) => continue,
                Ok(Some(ReceiveResponse {
                    response: Some(receive_response::Response::LastChunk(_)),
                })) => panic!("a dead session's collection must not complete"),
                Ok(Some(_)) | Ok(None) | Err(_) => break,
            }
        }

        // The next registration collects the parked bundle whole.
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let announced = delivery(&mut app, adu.len() as u64).await;
        let collected = collect(
            &mut harness.client,
            app.token.clone(),
            &announced.bundle_id,
            false,
        )
        .await
        .unwrap();
        assert_eq!(collected, adu);

        harness.bpa.shutdown().await;
    }

    // Only the ack completes a delivery: a cancel after the final
    // chunk still abandons, and the bundle is re-announced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_after_the_last_chunk_parks_the_delivery() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = b"taken but not committed";
        let destination = app.endpoint_id.clone();
        send(&mut harness.client, app.token.clone(), &destination, adu)
            .await
            .unwrap();
        let announced = delivery(&mut app, adu.len() as u64).await;

        let (requests_tx, mut stream, collected) =
            take_all(&mut harness.client, app.token.clone(), &announced.bundle_id).await;
        assert_eq!(collected, adu);

        // The last chunk is in hand but not acked: the cancel abandons
        // the delivery and the server closes cleanly.
        requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Cancel(())),
            })
            .await
            .unwrap();
        clean_end(&mut stream).await;

        recollected_after_reregistration(&mut harness, app, adu).await;

        harness.bpa.shutdown().await;
    }

    // A client that takes the whole ADU and then ends its request
    // stream without an ack commits nothing, and the bundle is
    // re-announced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_receipt_without_an_ack_parks_the_delivery() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = b"taken then declined";
        let destination = app.endpoint_id.clone();
        send(&mut harness.client, app.token.clone(), &destination, adu)
            .await
            .unwrap();
        let announced = delivery(&mut app, adu.len() as u64).await;

        let (requests_tx, mut stream, collected) =
            take_all(&mut harness.client, app.token.clone(), &announced.bundle_id).await;
        assert_eq!(collected, adu);

        // A request stream that ends without an ack is an abandonment.
        drop(requests_tx);
        assert_eq!(terminal_status(&mut stream).await.code(), Code::Cancelled);

        recollected_after_reregistration(&mut harness, app, adu).await;

        harness.bpa.shutdown().await;
    }

    // An ack before the final chunk is a protocol violation: the
    // collection ends without its last chunk and the bundle is
    // re-announced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ack_before_the_final_chunk_never_commits() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // More than the response buffer plus the transport windows can
        // absorb, so the transfer cannot complete before the early ack
        // is seen. Derived from the constant so a deeper buffer cannot
        // make the test vacuous.
        let adu = vec![0x5a; DATA_CHANNEL_DEPTH * 4 * crate::CHUNK_SIZE];
        let destination = app.endpoint_id.clone();
        send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
        let announced = delivery(&mut app, adu.len() as u64).await;

        // Ack straight after the metadata, before reading a single chunk.
        let (requests_tx, requests_rx) = mpsc::channel(2);
        requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: app.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Ack(())),
            })
            .await
            .unwrap();
        let mut stream = harness
            .client
            .receive(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();

        // The stream must end without its last chunk. The
        // INVALID_ARGUMENT refusal is best-effort: it goes out via
        // `try_send` against a possibly full response channel, so the
        // call may end as a bare truncation instead.
        loop {
            match timeout(stream.message()).await {
                Ok(Some(ReceiveResponse {
                    response: Some(receive_response::Response::Chunk(_)),
                })) => continue,
                Ok(Some(ReceiveResponse {
                    response: Some(receive_response::Response::LastChunk(_)),
                })) => panic!("an early ack must never commit"),
                Ok(Some(_)) | Ok(None) | Err(_) => break,
            }
        }

        recollected_after_reregistration(&mut harness, app, &adu).await;

        harness.bpa.shutdown().await;
    }

    // A client that claims a large collection and stops reading must
    // not wedge pool shutdown: the parked writer abandons its terminal
    // status rather than await a channel nothing drains.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_shutdown_survives_a_claimed_unread_receive() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = vec![0x5a; 16 * crate::CHUNK_SIZE];
        let destination = app.endpoint_id.clone();
        send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
        let announced = delivery(&mut app, adu.len() as u64).await;

        // Claim the collection and read nothing, keeping the call (and
        // its connection) alive across the shutdown.
        let (requests_tx, requests_rx) = mpsc::channel(2);
        requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: app.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        let _claimed = harness
            .client
            .receive(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();

        // Shutdown must drain despite the parked, unread writer.
        timeout(harness.tasks.shutdown()).await;

        harness.bpa.shutdown().await;
    }

    // A subscription parked on its first read has no session behind it
    // yet, so the pool is the only thing that can reach it: shutdown
    // must end the call rather than wait on a client that never
    // registers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unregistered_subscription_does_not_block_shutdown() {
        let harness = harness().await;
        // Subscribed before the call, so the barrier cannot be missed.
        let mut opened = harness.surface.hooks.opened.subscribe();

        // Held open and silent: the task parks on a Register that never
        // arrives.
        let (_requests_tx, requests_rx) = mpsc::channel::<SubscribeRequest>(1);
        let mut client = harness.client.clone();
        let subscribed =
            tokio::spawn(async move { client.subscribe(ReceiverStream::new(requests_rx)).await });
        timeout(opened.recv()).await.unwrap();

        timeout(harness.tasks.shutdown()).await;

        let Err(status) = timeout(subscribed).await.unwrap() else {
            panic!("shutdown must end a subscription that never registered");
        };
        assert_eq!(status.code(), Code::Unavailable);

        harness.bpa.shutdown().await;
    }

    // The feed lease bounds the client's silence mid-transfer: a Send
    // that stops after its metadata is ended by the lease, which also
    // closes the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_that_stops_feeding_loses_its_feed_lease() {
        let mut harness = harness_with_limits(
            ipn1(),
            Limits {
                feed: Duration::ZERO,
                ..Limits::default()
            },
        )
        .await;
        let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        // Subscribed before the transfer: the lapsed lease closes the
        // session behind it, and that is the barrier.
        let mut torn = harness.surface.hooks.torn_down.subscribe();

        // Metadata and nothing more, with the request side held open, so
        // a zero lease can only expire where the client is genuinely
        // silent.
        let (requests_tx, requests_rx) = mpsc::channel(1);
        requests_tx
            .send(SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: app.token.clone(),
                    destination: app.endpoint_id.clone(),
                    lifetime: Some(prost_types::Duration {
                        seconds: 3600,
                        nanos: 0,
                    }),
                    options: None,
                    adu_size: None,
                })),
            })
            .await
            .unwrap();

        let status = timeout(harness.client.send(ReceiverStream::new(requests_rx)))
            .await
            .expect_err("a client that stops feeding must lose the transfer");
        assert_eq!(status.code(), Code::DeadlineExceeded);
        wait_torn_down(&mut torn, &app.token).await;

        drop(requests_tx);
        harness.bpa.shutdown().await;
    }

    // A stalled session must not starve the pipeline: other
    // registrations keep delivering.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_session_does_not_starve_other_registrations() {
        let mut harness = harness().await;
        // Registered, then never read again: its event buffer fills.
        let stalled = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let mut healthy = register(&mut harness.client, Some(register::ServiceId::Ipn(8))).await;

        // Past the session event buffer, derived from the constant so
        // a larger buffer cannot make the flood vacuous.
        let depth = EVENT_DEPTH;
        let flood = depth + depth / 2;
        for i in 0..flood {
            send(
                &mut harness.client,
                healthy.token.clone(),
                &stalled.endpoint_id,
                format!("flood {i}").as_bytes(),
            )
            .await
            .unwrap();
        }

        // The healthy endpoint still delivers.
        let adu = b"alive";
        let destination = healthy.endpoint_id.clone();
        send(
            &mut harness.client,
            healthy.token.clone(),
            &destination,
            adu,
        )
        .await
        .unwrap();
        let announced = delivery(&mut healthy, adu.len() as u64).await;
        let collected = collect(
            &mut harness.client,
            healthy.token.clone(),
            &announced.bundle_id,
            false,
        )
        .await
        .unwrap();
        assert_eq!(collected, adu);

        // Dropping the stalled session frees its parked announcements,
        // so shutdown can drain.
        drop(stalled);
        harness.bpa.shutdown().await;
    }

    // A dtn-scheme registration needs a dtn node id: on an ipn-only
    // node it fails the handshake with FAILED_PRECONDITION.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dtn_registration_needs_a_dtn_node_id() {
        let mut harness = harness().await;

        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id: Some(register::ServiceId::Dtn("mail".to_string())),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dtn_registration_binds_the_dtn_endpoint() {
        let node_ids = NodeIds::try_from(
            [
                NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 1,
                }),
                NodeId::Dtn(DtnNodeId {
                    node_name: "node1".into(),
                }),
            ]
            .as_slice(),
        )
        .unwrap();
        let mut harness = harness_with(node_ids).await;

        let app = register(
            &mut harness.client,
            Some(register::ServiceId::Dtn("mail".to_string())),
        )
        .await;
        assert_eq!(app.endpoint_id, "dtn://node1/mail");

        harness.bpa.shutdown().await;
    }

    // An application behind the client SDK: deliveries are pulled to
    // completion through the announced stream and recorded.
    #[cfg(feature = "client")]
    struct SdkApp {
        sink: Once<Box<dyn ApplicationSink>>,
        delivered: mpsc::Sender<(Eid, Bytes)>,
        statuses: mpsc::Sender<(BundleId, services::StatusNotify)>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl services::Application for SdkApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            bundle_id: &BundleId,
            _expiry: OffsetDateTime,
            _ack_requested: bool,
            _adu_size: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let payload = concat_stream(stream, usize::MAX, None).await?;
            let _ = self
                .delivered
                .send((bundle_id.source.clone(), payload))
                .await;
            Ok(())
        }

        async fn on_status_notify(
            &self,
            bundle_id: &BundleId,
            _from: &Eid,
            kind: services::StatusNotify,
            _reason: status_report::ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
            let _ = self.statuses.send((bundle_id.clone(), kind)).await;
        }
    }

    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_sdk_roundtrip() {
        let harness = harness().await;
        let client = BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
        let (statuses_tx, _statuses_rx) = mpsc::channel(4);
        let app = Arc::new(SdkApp {
            sink: Once::new(),
            delivered: delivered_tx,
            statuses: statuses_tx,
        });
        let handle = client
            .register_application(Service::Ipn(9), app.clone())
            .await
            .unwrap();
        let eid = handle.id().clone();
        assert_eq!(eid.to_string(), "ipn:1.9");

        let adu = Bytes::from_static(b"through the sdk and back");
        let sink = app.sink.get().unwrap();
        sink.send(
            eid.clone(),
            Duration::from_secs(3600),
            None,
            None,
            &mut adu.clone(),
        )
        .await
        .unwrap();

        let (source, payload) = timeout(delivered_rx.recv()).await.unwrap();
        assert_eq!(source, eid);
        assert_eq!(payload, adu);

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }

    // An application that echoes each delivery back out through its own
    // sink from inside `on_deliver`.
    #[cfg(feature = "client")]
    struct EchoApp {
        sink: Once<Box<dyn ApplicationSink>>,
        // Where each received delivery is echoed to.
        peer: Eid,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl services::Application for EchoApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            _bundle_id: &BundleId,
            _expiry: OffsetDateTime,
            _ack_requested: bool,
            _adu_size: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let mut payload = concat_stream(stream, usize::MAX, None).await?;
            // The reply is issued from inside the delivery, on the delivery
            // task: the SDK must not serialise the two against each other.
            self.sink
                .get()
                .unwrap()
                .send(
                    self.peer.clone(),
                    Duration::from_secs(3600),
                    None,
                    None,
                    &mut payload,
                )
                .await?;
            Ok(())
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &BundleId,
            _from: &Eid,
            _kind: services::StatusNotify,
            _reason: status_report::ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
        }
    }

    // Replying from within a pulled delivery must not deadlock, even
    // with more echoes in flight than the SDK's concurrency bound.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_sdk_reply_from_within_a_delivery_does_not_deadlock() {
        let harness = harness().await;
        let client = BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        // The collector both seeds the echo app and receives every echo.
        let (echoed_tx, mut echoed_rx) = mpsc::channel(64);
        let (statuses_tx, _statuses_rx) = mpsc::channel(4);
        let collector = Arc::new(SdkApp {
            sink: Once::new(),
            delivered: echoed_tx,
            statuses: statuses_tx,
        });
        let collector_handle = client
            .register_application(Service::Ipn(8), collector.clone())
            .await
            .unwrap();
        let collector_eid = collector_handle.id().clone();

        let echo = Arc::new(EchoApp {
            sink: Once::new(),
            peer: collector_eid.clone(),
        });
        let echo_handle = client
            .register_application(Service::Ipn(9), echo.clone())
            .await
            .unwrap();
        let echo_eid = echo_handle.id().clone();

        // Far more than the SDK's concurrent-delivery bound, so replies are
        // generated from several deliveries in flight at once.
        let count = MAX_CONCURRENT_DELIVERIES.get() * 8;
        let seed = collector.sink.get().unwrap();
        for i in 0..count {
            seed.send(
                echo_eid.clone(),
                Duration::from_secs(3600),
                None,
                None,
                &mut Bytes::from(format!("echo {i}")),
            )
            .await
            .unwrap();
        }

        // A reply that deadlocked inside a delivery would leave this
        // short; the timeout only bounds a regression.
        let mut received = 0;
        while received < count {
            let (source, _payload) = timeout(echoed_rx.recv()).await.unwrap();
            assert_eq!(source, echo_eid);
            received += 1;
        }

        seed.unregister().await;
        echo.sink.get().unwrap().unregister().await;
        harness.bpa.shutdown().await;
    }

    // An application that receives each delivery whole, then declines it.
    #[cfg(feature = "client")]
    struct DecliningApp {
        sink: Once<Box<dyn ApplicationSink>>,
        // Carries the fully received payload of each declined delivery.
        declined: mpsc::Sender<Bytes>,
        unregistered: mpsc::Sender<()>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl services::Application for DecliningApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {
            let _ = self.unregistered.send(()).await;
        }

        async fn on_deliver(
            &self,
            _bundle_id: &BundleId,
            _expiry: OffsetDateTime,
            _ack_requested: bool,
            _adu_size: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let payload = concat_stream(stream, usize::MAX, None).await?;
            let _ = self.declined.send(payload).await;
            Err(services::Error::Internal("declined".into()))
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &BundleId,
            _from: &Eid,
            _kind: services::StatusNotify,
            _reason: status_report::ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
        }
    }

    // An application that buffers the whole ADU and then returns `Err`
    // never commits: no ack goes out, and the bundle stays parked.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_sdk_decline_after_full_receipt_is_redelivered() {
        let harness = harness().await;
        let client = BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let (declined_tx, mut declined_rx) = mpsc::channel(4);
        let (unregistered_tx, mut unregistered_rx) = mpsc::channel(1);
        let decliner = Arc::new(DecliningApp {
            sink: Once::new(),
            declined: declined_tx,
            unregistered: unregistered_tx,
        });
        let handle = client
            .register_application(Service::Ipn(9), decliner.clone())
            .await
            .unwrap();
        let eid = handle.id().clone();

        let adu = Bytes::from_static(b"declined then redelivered");
        let sink = decliner.sink.get().unwrap();
        sink.send(
            eid.clone(),
            Duration::from_secs(3600),
            None,
            None,
            &mut adu.clone(),
        )
        .await
        .unwrap();

        // The decliner received the whole ADU before refusing it.
        let payload = timeout(declined_rx.recv()).await.unwrap();
        assert_eq!(payload, adu);

        // `on_unregister` fires only after the server has freed the
        // identity, so the re-registration below cannot race it.
        sink.unregister().await;
        timeout(unregistered_rx.recv()).await.unwrap();

        // The parked bundle is announced to the accepting registration,
        // which collects and commits it whole.
        let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
        let (statuses_tx, _statuses_rx) = mpsc::channel(4);
        let app = Arc::new(SdkApp {
            sink: Once::new(),
            delivered: delivered_tx,
            statuses: statuses_tx,
        });
        let _accepting = client
            .register_application(Service::Ipn(9), app.clone())
            .await
            .unwrap();
        let (_, payload) = timeout(delivered_rx.recv()).await.unwrap();
        assert_eq!(payload, adu);

        app.sink.get().unwrap().unregister().await;
        harness.bpa.shutdown().await;
    }

    // A requested delivery report round-trips to the sending
    // application through the wire.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_delivery_report_reaches_the_sending_application() {
        let harness = harness().await;
        let client = BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
        let (statuses_tx, mut statuses_rx) = mpsc::channel(4);
        let app = Arc::new(SdkApp {
            sink: Once::new(),
            delivered: delivered_tx,
            statuses: statuses_tx,
        });
        let handle = client
            .register_application(Service::Ipn(9), app.clone())
            .await
            .unwrap();
        let eid = handle.id().clone();

        let sink = app.sink.get().unwrap();
        let sent = sink
            .send(
                eid.clone(),
                Duration::from_secs(3600),
                Some(services::SendOptions {
                    notify_delivery: true,
                    ..Default::default()
                }),
                None,
                &mut Bytes::from_static(b"report me"),
            )
            .await
            .unwrap();

        // The SdkApp pulls the delivery to completion, which is what
        // generates the delivered report.
        let _ = timeout(delivered_rx.recv()).await.unwrap();

        let (reported, kind) = timeout(statuses_rx.recv()).await.unwrap();
        assert_eq!(reported, sent);
        assert_eq!(kind, services::StatusNotify::Delivered);

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }

    #[ignore = "the BPA serialises deliveries per service, so a held-open collection blocks the next announcement: see docs/TODO.md"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn re_registration_re_announces_many_parked_deliveries() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Announced to this session but never collected. Interleaved
        // send/event pairs keep the event buffer shallow.
        const PARKED: usize = 48;
        let destination = app.endpoint_id.clone();
        for i in 0..PARKED {
            send(
                &mut harness.client,
                app.token.clone(),
                &destination,
                format!("parked {i}").as_bytes(),
            )
            .await
            .unwrap();
            let event = timeout(app.events.message()).await.unwrap().unwrap();
            assert!(
                matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "expected a Delivery, got {event:?}"
            );
        }

        app.requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(timeout(app.events.message()).await.unwrap().is_none());

        // Re-registration must complete, and every parked bundle be
        // announced again to the new session.
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let mut announced = std::collections::HashSet::new();
        let mut last = String::new();
        while announced.len() < PARKED {
            let event = timeout(app.events.message()).await.unwrap().unwrap();
            let Some(subscribe_response::Event::Delivery(delivery)) = event.event else {
                panic!("expected a Delivery, got {event:?}");
            };
            last = delivery.bundle_id.clone();
            announced.insert(delivery.bundle_id);
        }
        assert_eq!(announced.len(), PARKED);

        // The re-announced bundles are collectable.
        let collected = collect(&mut harness.client, app.token.clone(), &last, false)
            .await
            .unwrap();
        assert!(collected.starts_with(b"parked "));

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unregister_ends_the_session_and_invalidates_the_token() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let mut torn = harness.surface.hooks.torn_down.subscribe();
        app.requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(
            timeout(app.events.message()).await.unwrap().is_none(),
            "unregister must end the session stream"
        );

        // Teardown runs after the stream closes; waiting on the signal
        // makes the rejection below race-free.
        wait_torn_down(&mut torn, &app.token).await;

        let status = send(&mut harness.client, app.token.clone(), "ipn:1.7", b"stale")
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    // A BPA stub whose `register_application` hands over a sink,
    // signals, then parks until released. Only that method is
    // reachable.
    struct ParkedBpa {
        registered: mpsc::Sender<()>,
        release: Arc<Notify>,
        unregistered: mpsc::Sender<()>,
    }

    struct ParkedSink {
        unregistered: mpsc::Sender<()>,
    }

    #[async_trait]
    impl ApplicationSink for ParkedSink {
        async fn unregister(&self) {
            let _ = self.unregistered.send(()).await;
        }

        async fn send(
            &self,
            _destination: Eid,
            _lifetime: Duration,
            _options: Option<SendOptions>,
            _size_hint: Option<u64>,
            _stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<BundleId> {
            unreachable!("the abandoned subscription never reaches a send")
        }
    }

    #[async_trait]
    impl BpaRegistration for ParkedBpa {
        async fn register_application(
            &self,
            _service_id: Service,
            application: Arc<dyn services::Application>,
        ) -> services::Result<Eid> {
            application
                .on_register(
                    &"ipn:1.7".parse().unwrap(),
                    Box::new(ParkedSink {
                        unregistered: self.unregistered.clone(),
                    }),
                )
                .await;
            self.registered.send(()).await.unwrap();
            // The registration is committed; the rpc can be abandoned
            // from here, and this call still completes.
            self.release.notified().await;
            Ok("ipn:1.7".parse().unwrap())
        }

        async fn register_cla(
            &self,
            _name: String,
            _cla: Arc<dyn Cla>,
            _policy: Option<Arc<dyn FlowControllerFactory>>,
            _init: ClaInit,
        ) -> cla::Result<Vec<NodeId>> {
            unreachable!("the application surface registers applications only")
        }

        async fn register_service(
            &self,
            _service_id: Service,
            _service: Arc<dyn BpaService>,
        ) -> services::Result<Eid> {
            unreachable!("the application surface registers applications only")
        }

        async fn register_dynamic_service(
            &self,
            _service: Arc<dyn BpaService>,
        ) -> services::Result<Eid> {
            unreachable!("the application surface registers applications only")
        }

        async fn register_dynamic_application(
            &self,
            _application: Arc<dyn services::Application>,
        ) -> services::Result<Eid> {
            unreachable!("this test registers an explicit service id")
        }

        async fn register_routing_agent(
            &self,
            _name: String,
            _agent: Arc<dyn RoutingAgent>,
        ) -> routing::Result<Vec<NodeId>> {
            unreachable!("the application surface registers applications only")
        }
    }

    // Fires when the rpc handler future is dropped rather than
    // completed: the only signal tonic exposes that the server
    // processed the client's reset. The test needs it to order the
    // reset before the registration returns.
    struct HandlerDropped(Option<mpsc::Sender<()>>);

    impl Drop for HandlerDropped {
        fn drop(&mut self) {
            if let Some(dropped_tx) = self.0.take() {
                let _ = dropped_tx.try_send(());
            }
        }
    }

    // Wraps the surface to hold a `HandlerDropped` across `subscribe`.
    #[derive(Clone)]
    struct WatchedApplication {
        inner: ApplicationServiceImpl,
        dropped_tx: mpsc::Sender<()>,
    }

    #[async_trait]
    impl ApplicationService for WatchedApplication {
        type SubscribeStream = SessionStream<SubscribeResponse>;
        type ReceiveStream = ReceiverStream<Result<ReceiveResponse, Status>>;

        async fn subscribe(
            &self,
            request: Request<Streaming<SubscribeRequest>>,
        ) -> Result<Response<Self::SubscribeStream>, Status> {
            let mut dropped = HandlerDropped(Some(self.dropped_tx.clone()));
            let response = self.inner.subscribe(request).await;
            // The handler completed; disarm the drop signal.
            dropped.0 = None;
            response
        }

        async fn send(
            &self,
            request: Request<Streaming<SendRequest>>,
        ) -> Result<Response<SendResponse>, Status> {
            self.inner.send(request).await
        }

        async fn receive(
            &self,
            request: Request<Streaming<ReceiveRequest>>,
        ) -> Result<Response<Self::ReceiveStream>, Status> {
            self.inner.receive(request).await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_rpc_abandoned_during_registration_ends_unregistered() {
        let (registered_tx, mut registered_rx) = mpsc::channel(1);
        let (unregistered_tx, mut unregistered_rx) = mpsc::channel(1);
        let release = Arc::new(Notify::new());

        let (dropped_tx, mut dropped_rx) = mpsc::channel(1);

        let tasks = TaskPool::new();
        let service = ApplicationServiceServer::new(WatchedApplication {
            inner: ApplicationServiceImpl::new(
                Arc::new(ParkedBpa {
                    registered: registered_tx,
                    release: release.clone(),
                    unregistered: unregistered_tx,
                }),
                tasks.clone(),
            ),
            dropped_tx,
        });
        let address = serve(Server::builder().add_service(service)).await;
        let mut client = ApplicationServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();

        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id: Some(register::ServiceId::Ipn(7)),
                })),
            })
            .await
            .unwrap();
        let call =
            tokio::spawn(async move { client.subscribe(ReceiverStream::new(requests_rx)).await });

        // The registration has committed and the register call is
        // parked, exactly where nothing else watches the subscription.
        timeout(registered_rx.recv()).await.unwrap();

        // Aborting resets the http/2 stream, so the handler waiting for
        // the subscription is dropped.
        call.abort();
        drop(requests_tx);

        // The reset must reach the server before registration returns,
        // or the window is never entered.
        timeout(dropped_rx.recv()).await.unwrap();

        // Let the registration complete; the rpc that would receive
        // the subscription is gone.
        release.notify_one();

        // The registration made by the abandoned rpc must still be
        // unregistered.
        timeout(unregistered_rx.recv()).await.unwrap();

        tasks.shutdown().await;
    }

    // An uncollected delivery expires its claim lease and ends the
    // session. A zero lease can only expire while nothing collects,
    // and session teardown is the event the test waits on.
    #[tokio::test]
    async fn an_uncollected_delivery_expires_its_claim_and_ends_the_session() {
        let harness = harness_with_limits(
            ipn1(),
            Limits {
                claim: Duration::ZERO,
                ..Limits::default()
            },
        )
        .await;
        let mut torn = harness.surface.hooks.torn_down.subscribe();
        let mut client = harness.client.clone();
        let app = register(&mut client, Some(register::ServiceId::Ipn(9))).await;

        // Delivered to itself, so the bundle is offered to the very
        // session that will not collect it.
        send(
            &mut client,
            app.token.clone(),
            &app.endpoint_id,
            b"never collected",
        )
        .await
        .unwrap();

        // The client holds its stream open and never unregisters, so
        // only the expired claim can reach this barrier.
        wait_torn_down(&mut torn, &app.token).await;

        harness.bpa.shutdown().await;
        drop(harness.tasks);
    }

    // A collection drained to its last chunk but never acked: the ack
    // lease expires, the call ends with DEADLINE_EXCEEDED, and the
    // session closes.
    #[tokio::test]
    async fn a_delivery_left_unacked_expires_its_ack_lease_and_ends_the_session() {
        let harness = harness_with_limits(
            ipn1(),
            Limits {
                ack: Duration::ZERO,
                ..Limits::default()
            },
        )
        .await;
        let mut torn = harness.surface.hooks.torn_down.subscribe();
        let mut client = harness.client.clone();
        let mut app = register(&mut client, Some(register::ServiceId::Ipn(9))).await;

        let adu = b"collected but never acked";
        send(&mut client, app.token.clone(), &app.endpoint_id, adu)
            .await
            .unwrap();
        let delivery = delivery(&mut app, adu.len() as u64).await;

        // The whole ADU was drained, so the response channel has room
        // for the terminal status.
        let (collected, status) =
            collect_unacked(&mut client, app.token.clone(), &delivery.bundle_id).await;
        assert_eq!(collected, adu);
        assert_eq!(status.code(), Code::DeadlineExceeded);

        wait_torn_down(&mut torn, &app.token).await;

        harness.bpa.shutdown().await;
        drop(harness.tasks);
    }

    // Collects an announced delivery whole, sends no ack, and returns
    // the ADU plus the status the server ended the call with.
    async fn collect_unacked(
        client: &mut ApplicationServiceClient<Channel>,
        token: Bytes,
        bundle_id: &str,
    ) -> (Vec<u8>, Status) {
        // Held open past the last chunk: half-closing the request side
        // would count as an ending, and the point is a silent client.
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(4);
        requests_tx
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: token,
                    bundle_id: bundle_id.to_string(),
                })),
            })
            .await
            .unwrap();

        let mut stream = client
            .receive(tokio_stream::wrappers::ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();
        let mut collected = Vec::new();
        loop {
            match timeout(stream.message()).await {
                Ok(Some(response)) => match response.response {
                    Some(receive_response::Response::Chunk(chunk))
                    | Some(receive_response::Response::LastChunk(chunk)) => {
                        collected.extend_from_slice(&chunk);
                    }
                    other => panic!("expected a chunk, got {other:?}"),
                },
                Ok(None) => panic!("the collection ended without a status"),
                Err(status) => return (collected, status),
            }
        }
    }
}
