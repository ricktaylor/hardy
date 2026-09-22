// The `hardy.service.v1` service. `Subscribe` opens a registration
// session, `Send` streams a whole encoded bundle into the BPA, and
// deliveries are announced on the session stream and collected by a
// `Receive` call.

use std::sync::Arc;

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::TaskPool;
use hardy_bpa::{
    Bytes, async_trait,
    bpa::BpaRegistration,
    services::{self, ServiceSink},
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
    grammar::{Ack, Cancel},
    server::{
        Limits,
        adapter::{RequestReader, ResponseWriter},
        announce::{Announcements, Collection},
        error::{self, Error},
        leases::Lease,
        session::{Session, SessionStream},
    },
    service::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse,
        Registration, SendMetadata, SendRequest, SendResponse, StatusAssertion, SubscribeRequest,
        SubscribeResponse, receive_request, register, send_request,
        service_service_server::ServiceService, subscribe_request, subscribe_response,
    },
    timestamp::to_timestamp,
    token::Token,
    transfer::Writer,
};

// The surface name used in spans, lease warnings, and the token `sub`.
const LABEL: &str = "service";

// A subscription's event buffer, in messages.
const EVENT_DEPTH: usize = 16;

// The per-session `services::Service` registered with the BPA.
struct GrpcService {
    session: Session<SubscribeResponse, Arc<dyn ServiceSink>>,
    deliveries: Announcements<ReceiveResponse, ReceiveRequest>,
}

impl GrpcService {
    fn new(session: Session<SubscribeResponse, Arc<dyn ServiceSink>>) -> Self {
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
impl services::Service for GrpcService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        self.session.register(Arc::from(sink));
    }

    async fn on_unregister(&self) {
        self.session.abort();
    }

    async fn on_deliver(
        &self,
        bundle_id: &BundleId,
        expiry: OffsetDateTime,
        bundle_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let announced = self.deliveries.announce(bundle_id).map_err(|e| {
            warn!("Refusing a second announcement for a bundle already being delivered");
            services::Error::from(e)
        })?;

        self.event(subscribe_response::Event::Delivery(Delivery {
            bundle_id: bundle_id.to_key(),
            expire_time: Some(to_timestamp(expiry)),
            bundle_size,
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

/// The server implementation of the `hardy.service.v1` service.
///
/// Clones share one set of sessions. Shut down the pool given to
/// [`new`](Self::new) only after the transport has stopped accepting:
/// pool shutdown tears down every subscription.
#[derive(Clone)]
pub struct ServiceServiceImpl {
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    limits: Limits,
    sessions: Arc<DashMap<Token, Arc<GrpcService>, RandomState>>,
    #[cfg(test)]
    hooks: super::tests::Hooks,
}

impl ServiceServiceImpl {
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
        let service_id = register.service_id.map(|id| match id {
            register::ServiceId::Ipn(n) => Service::Ipn(n),
            register::ServiceId::Dtn(demux) => Service::Dtn(demux.into()),
        });

        // Created before the BPA registration: events raised during
        // registration buffer in the session.
        let token = Token::mint(&session_sub(LABEL, service_id.as_ref()));
        let session = Session::new(self.tasks.child_token(), LABEL, self.limits, EVENT_DEPTH);
        let service = Arc::new(GrpcService::new(session));

        let endpoint_id = match self.register(service_id, service.clone()).await {
            Ok(endpoint_id) => endpoint_id,
            Err(e) => {
                let _ = response_tx.send(Err(service_status(e)));
                return;
            }
        };

        self.sessions.insert(token.clone(), service.clone());

        let registration = SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(Registration {
                endpoint_id: endpoint_id.to_string(),
                session_token: token.clone().into(),
            })),
        };
        let mut stream = service.session.open(registration);
        let _guard = stream.cancel_guard();

        if response_tx.send(Ok(Response::new(stream))).is_ok() {
            service.session.serve(requests).await;
        }

        // In order: stop the work, drop the token, unregister; the
        // stream ends last.
        service.session.abort();
        self.sessions.remove(&token);
        if let Some(sink) = service.session.registered() {
            sink.unregister().await;
        }
        #[cfg(test)]
        let _ = self.hooks.torn_down.send(token);
    }

    async fn register(
        &self,
        service_id: Option<Service>,
        service: Arc<GrpcService>,
    ) -> services::Result<Eid> {
        match service_id {
            Some(service_id) => self.bpa.register_service(service_id, service).await,
            None => self.bpa.register_dynamic_service(service).await,
        }
    }

    fn resolve(&self, token: Bytes) -> Result<Arc<GrpcService>, Status> {
        self.sessions
            .get(&Token::from(token))
            .map(|service| service.clone())
            .ok_or_else(|| Status::unauthenticated("Unknown session token"))
    }
}

#[async_trait]
impl ServiceService for ServiceServiceImpl {
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

        let Some(send_request::Request::Metadata(SendMetadata { session_token })) =
            requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be the metadata",
            ));
        };
        let service = self.resolve(session_token)?;

        let mut reader = RequestReader::new(requests, service.session.leases().clone(), "Send");
        match service
            .session
            .registered()
            .ok_or_else(|| service_status(services::Error::Disconnected))?
            .send(&mut reader)
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
        let service = self.resolve(session_token)?;
        let Some(responses_rx) = service.deliveries.collect(&bundle_id, requests) else {
            return Err(Status::not_found("No such delivery"));
        };
        Ok(Response::new(responses_rx))
    }
}

// Wire tests against a real BPA.
#[cfg(test)]
mod tests {
    #[cfg(feature = "client")]
    use std::borrow::Cow;
    use std::{net::SocketAddr, time::Duration};

    #[cfg(feature = "client")]
    use crate::client::BpaClient;
    #[cfg(feature = "client")]
    use hardy_async::sync::spin::Once;
    use hardy_bpa::bpa::Bpa;
    #[cfg(feature = "client")]
    use hardy_bpa::stream::concat_stream;
    #[cfg(feature = "client")]
    use hardy_bpv7::{builder::Builder, bundle, creation_timestamp::CreationTimestamp};
    use tonic::{
        Code,
        transport::{Channel, Server},
    };

    use super::{
        super::tests::{build_bpa, build_bundle, ipn1, serve, timeout, wait_torn_down},
        *,
    };
    use crate::service::{
        Register, Unregister, receive_response, service_service_client::ServiceServiceClient,
        service_service_server::ServiceServiceServer,
    };

    struct Harness {
        bpa: Arc<Bpa>,
        // Held live: dropping the pool would tear down the sessions.
        #[expect(dead_code, reason = "held for its liveness")]
        tasks: TaskPool,
        client: ServiceServiceClient<Channel>,
        #[cfg_attr(
            not(feature = "client"),
            expect(dead_code, reason = "read by the client SDK test")
        )]
        address: SocketAddr,
        // A second handle on the surface, for the teardown barrier.
        surface: ServiceServiceImpl,
    }

    // A running BPA (node ipn:1) behind the surface on a port-0
    // listener, plus a connected generated client.
    async fn harness() -> Harness {
        harness_with_limits(Limits::default()).await
    }

    async fn harness_with_limits(limits: Limits) -> Harness {
        // Status reports enabled: the delivery-report test needs them.
        let bpa = build_bpa(ipn1(), true).await;

        let tasks = TaskPool::new();
        let surface = ServiceServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits);
        let service = ServiceServiceServer::new(surface.clone());
        let address = serve(Server::builder().add_service(service)).await;

        let client = ServiceServiceClient::connect(format!("http://{address}"))
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

    struct Registered {
        requests_tx: mpsc::Sender<SubscribeRequest>,
        events: Streaming<SubscribeResponse>,
        endpoint_id: String,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(
        client: &mut ServiceServiceClient<Channel>,
        service_id: Option<register::ServiceId>,
    ) -> Registered {
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

        Registered {
            requests_tx,
            events,
            endpoint_id: registration.endpoint_id,
            token: registration.session_token,
        }
    }

    async fn send(
        client: &mut ServiceServiceClient<Channel>,
        token: Bytes,
        bundle: Bytes,
    ) -> Result<SendResponse, Status> {
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: token,
                })),
            },
            SendRequest {
                request: Some(send_request::Request::LastChunk(bundle)),
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
        client: &mut ServiceServiceClient<Channel>,
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

    // Waits for the Delivery announcing `bundle_size` bytes on the
    // session stream.
    async fn delivery(registered: &mut Registered, bundle_size: u64) -> Delivery {
        loop {
            let event = timeout(registered.events.message()).await.unwrap().unwrap();
            match event.event {
                Some(subscribe_response::Event::Delivery(delivery)) => {
                    assert_eq!(delivery.bundle_size, bundle_size);
                    return delivery;
                }
                // Status reports about the sent bundle may interleave.
                Some(subscribe_response::Event::BundleStatusReport(_)) => {}
                other => panic!("expected a Delivery, got {other:?}"),
            }
        }
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
        let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let bundle = build_bundle(
            &registered.endpoint_id,
            &registered.endpoint_id,
            b"a whole bundle over the v1 wire",
        );
        let sent = send(
            &mut harness.client,
            registered.token.clone(),
            bundle.clone(),
        )
        .await
        .unwrap();
        assert!(!sent.bundle_id.is_empty());

        let delivery = delivery(&mut registered, bundle.len() as u64).await;

        // Collection returns the bundle exactly as stored: the builder
        // emits canonical bytes, so they round-trip unchanged.
        let collected = collect(
            &mut harness.client,
            registered.token.clone(),
            &delivery.bundle_id,
            false,
        )
        .await
        .unwrap();
        assert_eq!(collected, bundle);

        // The completed collection consumed the delivery: the sent and
        // announced ids were the same real bundle id, and it is gone.
        assert_eq!(delivery.bundle_id, sent.bundle_id);
        let gone = collect(
            &mut harness.client,
            registered.token.clone(),
            &delivery.bundle_id,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(gone.code(), Code::NotFound);

        harness.bpa.shutdown().await;
    }

    // A zero claim lease: the uncollected delivery expires the claim,
    // which tears the session down. The teardown hook is the barrier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_uncollected_delivery_expires_its_claim_and_ends_the_session() {
        let mut harness = harness_with_limits(Limits {
            claim: Duration::ZERO,
            ..Limits::default()
        })
        .await;
        let mut torn = harness.surface.hooks.torn_down.subscribe();
        let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Sent to itself: the bundle is offered to this session, which
        // never collects it.
        let bundle = build_bundle(
            &registered.endpoint_id,
            &registered.endpoint_id,
            b"never collected",
        );
        let size = bundle.len() as u64;
        send(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap();

        // The announcement reaches the service; nothing collects it.
        delivery(&mut registered, size).await;

        // The client holds its stream open and never unregisters, so
        // only the expired claim can reach this barrier.
        wait_torn_down(&mut torn, &registered.token).await;

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_truncated_send_never_commits() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // A Chunk but no LastChunk: the half-close is a truncation.
        let bundle = build_bundle(&registered.endpoint_id, &registered.endpoint_id, b"cut");
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: registered.token.clone(),
                })),
            },
            SendRequest {
                request: Some(send_request::Request::Chunk(bundle)),
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
        while let Some(event) = registered.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "a truncated send must not deliver"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_send_is_discarded() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let bundle = build_bundle(&registered.endpoint_id, &registered.endpoint_id, b"undo");
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: registered.token.clone(),
                })),
            },
            SendRequest {
                request: Some(send_request::Request::Chunk(bundle)),
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
        while let Some(event) = registered.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "a cancelled send must not deliver"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_invalid_bundle_is_rejected() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let status = send(
            &mut harness.client,
            registered.token.clone(),
            Bytes::from_static(b"not a bundle"),
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_collection_defers_to_the_next_registration() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let payload = vec![0x5a; crate::CHUNK_SIZE + 3];
        let bundle = build_bundle(&registered.endpoint_id, &registered.endpoint_id, &payload);
        send(
            &mut harness.client,
            registered.token.clone(),
            bundle.clone(),
        )
        .await
        .unwrap();
        let first = delivery(&mut registered, bundle.len() as u64).await;

        // The in-band Cancel ends the collection cleanly without an
        // Ack; the bundle is not finalised.
        collect(
            &mut harness.client,
            registered.token.clone(),
            &first.bundle_id,
            true,
        )
        .await
        .expect("a cancelled collection ends cleanly");

        // An announcement can be collected once: a retry in this
        // session is NotFound.
        let spent = collect(
            &mut harness.client,
            registered.token.clone(),
            &first.bundle_id,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(spent.code(), Code::NotFound);

        // The next registration gets the bundle announced afresh and
        // can collect all of it.
        registered
            .requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(
            timeout(registered.events.message())
                .await
                .unwrap()
                .is_none()
        );

        let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let announced = delivery(&mut registered, bundle.len() as u64).await;
        let collected = collect(
            &mut harness.client,
            registered.token.clone(),
            &announced.bundle_id,
            false,
        )
        .await
        .unwrap();
        assert_eq!(collected, bundle);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forged_token_is_rejected() {
        let mut harness = harness().await;
        register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"denied");
        let status = send(&mut harness.client, Bytes::from_static(b"forged"), bundle)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forged_source_is_rejected() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // The source EID does not match the registration's endpoint.
        let bundle = build_bundle("ipn:1.99", "ipn:1.7", b"forged source");
        let status = send(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_stream_tears_the_session_down() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // The client vanishes without Unregister. Waiting on the
        // teardown signal makes the rejection below race-free.
        let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"stale");
        let mut torn = harness.surface.hooks.torn_down.subscribe();
        drop(registered.events);
        drop(registered.requests_tx);
        wait_torn_down(&mut torn, &registered.token).await;

        let status = send(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    // A `services::Service` driven through the client SDK; records
    // deliveries and status reports.
    #[cfg(feature = "client")]
    struct SdkService {
        sink: Once<Box<dyn ServiceSink>>,
        delivered: mpsc::Sender<Bytes>,
        statuses: mpsc::Sender<(BundleId, services::StatusNotify)>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl services::Service for SdkService {
        async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            _bundle_id: &BundleId,
            _expiry: OffsetDateTime,
            _bundle_size: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let data = concat_stream(stream, usize::MAX, None).await?;
            let _ = self.delivered.send(data).await;
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
        let client =
            BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

        let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
        let (statuses_tx, _statuses_rx) = mpsc::channel(4);
        let svc = Arc::new(SdkService {
            sink: Once::new(),
            delivered: delivered_tx,
            statuses: statuses_tx,
        });
        let handle = client
            .register_service(Service::Ipn(9), svc.clone())
            .await
            .unwrap();
        let eid = handle.id().clone();
        assert_eq!(eid.to_string(), "ipn:1.9");

        // A whole `Bytes` buffer travels as one final segment, chunked
        // on the wire.
        let bundle = build_bundle("ipn:1.9", "ipn:1.9", b"through the sdk as a whole bundle");
        let sink = svc.sink.get().unwrap();
        sink.send(&mut bundle.clone()).await.unwrap();

        let data = timeout(delivered_rx.recv()).await.unwrap();
        assert_eq!(data, bundle);

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }

    // A bundle flagged for a delivery report gets the report back at
    // the registration that sent it.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_delivery_report_reaches_the_sending_service() {
        let harness = harness().await;
        let client = BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
        let (statuses_tx, mut statuses_rx) = mpsc::channel(4);
        let svc = Arc::new(SdkService {
            sink: Once::new(),
            delivered: delivered_tx,
            statuses: statuses_tx,
        });
        let _handle = client
            .register_service(Service::Ipn(9), svc.clone())
            .await
            .unwrap();

        // A raw bundle to self, flagged for a delivery report;
        // report-to is the node's administrative endpoint.
        let (built, data) = Builder::new("ipn:1.9".parse().unwrap(), "ipn:1.9".parse().unwrap())
            .with_flags(bundle::Flags {
                delivery_report_requested: true,
                ..Default::default()
            })
            .with_report_to("ipn:1.0".parse().unwrap())
            .with_payload(Cow::Borrowed(b"report me"))
            .build(CreationTimestamp::now())
            .unwrap();

        let sink = svc.sink.get().unwrap();
        let sent = sink.send(&mut Bytes::from(data)).await.unwrap();
        assert_eq!(sent, built.primary.id);

        // Pulling the delivery to completion generates the delivered
        // report.
        let _ = timeout(delivered_rx.recv()).await.unwrap();

        let (reported, kind) = timeout(statuses_rx.recv()).await.unwrap();
        assert_eq!(reported, sent);
        assert_eq!(kind, services::StatusNotify::Delivered);

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unregister_ends_the_session_and_invalidates_the_token() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let mut torn = harness.surface.hooks.torn_down.subscribe();

        registered
            .requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(
            timeout(registered.events.message())
                .await
                .unwrap()
                .is_none(),
            "unregister must end the session stream"
        );

        // Teardown runs after the stream closes; waiting on the signal
        // makes the rejection below race-free.
        let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"stale");
        wait_torn_down(&mut torn, &registered.token).await;
        let status = send(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }
}
