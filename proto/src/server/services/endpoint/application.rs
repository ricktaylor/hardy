//! The `hardy.application.v1` API.

use core::time::Duration;
use std::sync::Arc;

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::{CancellationToken, TaskPool, sync::spin::Once};
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
use tokio::sync::broadcast;
use tokio::sync::{
    OwnedSemaphorePermit, Semaphore,
    mpsc::{self, WeakSender, channel},
    oneshot,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::warn;
#[cfg(feature = "instrument")]
use tracing::{Instrument, Span, instrument, trace_span};

use super::deliver;
use crate::{
    MAX_MESSAGE_SIZE, MAX_TRANSFER_SIZE,
    application::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse,
        Registration, SendMetadata, SendRequest, SendResponse, StatusAssertion, SubscribeRequest,
        SubscribeResponse,
        application_service_server::{ApplicationService, ApplicationServiceServer},
        receive_request, register, send_request, subscribe_response,
    },
    chunking::{BoundedChunkReceiver, ChunkReceiver, ChunkSize, TransferError},
    common::Sizes,
    server::{
        Limits, MAX_INBOUND_TRANSFERS,
        announce::{AnnounceError, Announcements},
        services::{
            EventStream, expect_first, expect_register, prepend, send_event, token_subject,
            wait_for_unregister,
        },
        status::service_status,
        watchdog::{Stage, Watchdog},
    },
    timestamp::to_timestamp,
    token::Token,
};

/// The API's name, used in token subjects, logs, and spans.
const LABEL: &str = "application";

/// The number of events a session stream buffers ahead of the client; one more
/// slot is reserved for the status the session ends with.
const EVENT_DEPTH: usize = 16;

/// What a session hands back once it has registered: the `Registration` event
/// and the receiving end of its event channel.
type Session = (
    Registration,
    mpsc::Receiver<Result<SubscribeResponse, Status>>,
);

/// The server side of one application session: the [`services::Application`]
/// the BPA holds, and the handler a data-plane call resolves its token to.
///
/// The BPA's callbacks become wire exchanges. `on_deliver` announces the
/// bundle, waits for the client's `Receive`, streams the ADU to it, and waits
/// for its `ack`; `on_status_notify` pushes a status report event;
/// `on_unregister` ends the session. The handler keeps only a weak sender for
/// the event stream, so a call that resolved the handler cannot hold the stream
/// open past the session task.
struct GrpcApplication {
    cancel: CancellationToken,
    watchdog: Arc<Watchdog>,
    chunk_size: ChunkSize,
    registered: Once<Box<dyn ApplicationSink>>,
    events_tx: WeakSender<Result<SubscribeResponse, Status>>,
    deliveries: Announcements<ReceiveResponse, ReceiveRequest>,
    /// The `Send` calls the session may have open at once.
    transfers: Semaphore,
}

impl GrpcApplication {
    /// Creates the handler of a session that `cancel` ends, `limits` bounds,
    /// `events_tx` feeds, and whose transfers chunk at `chunk_size`.
    fn new(
        cancel: CancellationToken,
        limits: Limits,
        chunk_size: ChunkSize,
        events_tx: WeakSender<Result<SubscribeResponse, Status>>,
    ) -> Self {
        let watchdog = Arc::new(Watchdog::new(limits));
        Self {
            deliveries: Announcements::new(cancel.clone(), watchdog.clone()),
            registered: Once::new(),
            events_tx,
            watchdog,
            chunk_size,
            cancel,
            transfers: Semaphore::new(MAX_INBOUND_TRANSFERS),
        }
    }

    /// Pushes one event onto the session stream.
    async fn send_event(&self, event: subscribe_response::Event) -> Result<(), Status> {
        send_event(
            &self.events_tx,
            &self.cancel,
            &self.watchdog,
            SubscribeResponse { event: Some(event) },
        )
        .await
    }
}

#[async_trait]
impl services::Application for GrpcApplication {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        self.registered.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        self.cancel.cancel();
    }

    /// Announces the bundle, then hands it to [`deliver`], which streams the ADU
    /// to the client's `Receive` call and waits for the client to commit it with
    /// an `ack` or abandon it.
    ///
    /// # Errors
    ///
    /// Returns the BPA's error, so that the bundle is kept: a stall or a closed
    /// session as `Disconnected`, anything else as `StreamCancelled`.
    async fn on_deliver(
        &self,
        bundle_id: &BundleId,
        expiry: OffsetDateTime,
        ack_requested: bool,
        adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let delivery = subscribe_response::Event::Delivery(Delivery {
            bundle_id: bundle_id.to_key(),
            source: bundle_id.source.to_string(),
            expire_time: Some(to_timestamp(expiry)),
            ack_requested,
            adu_size,
        });
        let collection = self
            .deliveries
            .announce(bundle_id, async move || self.send_event(delivery).await)
            .await
            .map_err(|error| match error {
                AnnounceError::AlreadyAnnounced => {
                    warn!("refusing duplicate announcement: bundle already being delivered");
                    services::Error::StreamCancelled
                }
                AnnounceError::SessionClosed | AnnounceError::CollectionTimedOut => {
                    services::Error::Disconnected
                }
            })?;

        deliver(
            collection,
            stream,
            &self.cancel,
            &self.watchdog,
            self.chunk_size,
        )
        .await
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
            .send_event(subscribe_response::Event::BundleStatusReport(
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

/// The `hardy.application.v1` API of a BPA.
///
/// Implements the generated tonic trait [`ApplicationService`] over a
/// [`BpaRegistration`]. Each `Subscribe` registers a proxy
/// [`services::Application`] with the BPA and runs a session task on the API's
/// [`TaskPool`]; `Send` and `Receive` present the session token and move ADUs.
/// A host wraps the API in
/// [`ApplicationServiceServer`](crate::application::application_service_server::ApplicationServiceServer)
/// and adds it to a tonic server; the [`server`](crate::server) module shows
/// how.
///
/// Clones share one session index and one session ceiling, so an API may be
/// cloned freely. Shutting the pool down ends every session and the host should
/// do so when the tonic server stops.
#[derive(Clone)]
pub struct ApplicationServiceImpl {
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    limits: Limits,
    slots: Arc<Semaphore>,
    sessions: Arc<DashMap<Token, Arc<GrpcApplication>, RandomState>>,
    #[cfg(test)]
    opened: broadcast::Sender<()>,
    #[cfg(test)]
    closed: broadcast::Sender<()>,
}

impl ApplicationServiceImpl {
    /// Creates the API over `bpa` with the default [`Limits`], running its sessions
    /// on `tasks`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self::with_limits(bpa, tasks, Limits::default())
    }

    /// Creates the API over `bpa` with `limits`, running its sessions on `tasks`.
    pub fn with_limits(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool, limits: Limits) -> Self {
        Self {
            bpa,
            tasks,
            limits,
            slots: Arc::new(Semaphore::new(
                limits.max_sessions.get().min(Semaphore::MAX_PERMITS),
            )),
            sessions: Arc::new(DashMap::with_hasher(RandomState::default())),
            #[cfg(test)]
            opened: broadcast::channel(16).0,
            #[cfg(test)]
            closed: broadcast::channel(16).0,
        }
    }

    /// Registers `application` with the BPA under `service_id`, or under a
    /// service number of the BPA's choosing if `None`, and returns the endpoint
    /// id.
    async fn register(
        &self,
        service_id: Option<Service>,
        application: Arc<GrpcApplication>,
    ) -> services::Result<Eid> {
        match service_id {
            Some(service_id) => self.bpa.register_application(service_id, application).await,
            None => self.bpa.register_dynamic_application(application).await,
        }
    }

    /// Wraps the API in its generated tonic server, sized for the wire
    /// contract.
    ///
    /// The server accepts and sends messages of up to [`MAX_MESSAGE_SIZE`]
    /// bytes, as the client SDK does, so a chunk of any size a session can
    /// negotiate fits. A host that mounts the API another way must apply the
    /// same bounds.
    pub fn into_server(self) -> ApplicationServiceServer<Self> {
        ApplicationServiceServer::new(self)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
    }

    /// Resolves a session token to its live handler.
    ///
    /// # Errors
    ///
    /// Returns `UNAUTHENTICATED` if no live session holds `token`.
    fn resolve(&self, token: Bytes) -> Result<Arc<GrpcApplication>, Status> {
        self.sessions
            .get(&Token::from(token))
            .map(|application| application.clone())
            .ok_or_else(|| Status::unauthenticated("unknown session token"))
    }

    /// Runs one session from registration to its end.
    ///
    /// Registers the handler with the BPA, indexes its token, hands the
    /// `Registration` and the event channel back through `registration_tx`, and
    /// then waits for whichever comes first: the client dropping the response
    /// stream, a stall, server shutdown, the BPA unregistering the handler, or the
    /// client's `Unregister` or half-close. The exit sequence is then fixed: cancel
    /// the session's token, retire it from the index, and unregister the sink from
    /// the BPA. Only then is an ending the client did not ask for written as the
    /// stream's final status, into the slot reserved on the event channel so that a
    /// full buffer cannot swallow it, and the session slot released. By the time
    /// the client sees the stream end the token is dead and the endpoint id is
    /// free again.
    async fn run_session(
        self,
        slot: OwnedSemaphorePermit,
        service_id: Option<Service>,
        chunk_size: ChunkSize,
        mut requests: Streaming<SubscribeRequest>,
        registration_tx: oneshot::Sender<Result<Session, Status>>,
    ) {
        let (events_tx, events_rx) = channel(EVENT_DEPTH + 1);
        let permit = events_tx
            .clone()
            .try_reserve_owned()
            .expect("a freshly created channel has spare capacity");
        let token = Token::mint(&token_subject(LABEL, service_id.as_ref()));
        let child = self.tasks.child_token();
        let application = Arc::new(GrpcApplication::new(
            child.clone(),
            self.limits,
            chunk_size,
            events_tx.downgrade(),
        ));

        let endpoint_id = match self.register(service_id, application.clone()).await {
            Ok(endpoint_id) => endpoint_id,
            Err(e) => {
                let _ = registration_tx.send(Err(service_status(e)));
                return;
            }
        };

        self.sessions.insert(token.clone(), application.clone());

        let registration = Registration {
            endpoint_id: endpoint_id.to_string(),
            session_token: token.clone().into(),
            sizes: Some(Sizes {
                max_message_size: MAX_MESSAGE_SIZE as u64,
                chunk_size: chunk_size.get() as u64,
                max_transfer_size: MAX_TRANSFER_SIZE,
            }),
        };
        let result = if registration_tx.send(Ok((registration, events_rx))).is_ok() {
            tokio::select! {
                biased;
                _ = events_tx.closed() => Ok(()),
                _ = application.watchdog.stalled() => Err(Status::deadline_exceeded("timed out waiting for the client")),
                _ = self.tasks.cancel_token().cancelled() => {
                    Err(Status::unavailable("server shutting down"))
                }
                _ = child.cancelled() => Err(Status::unavailable("registration closed")),
                unregister = wait_for_unregister(&mut requests) => unregister,
            }
        } else {
            Ok(())
        };

        child.cancel();
        self.sessions.remove(&token);
        if let Some(sink) = application.registered.get() {
            sink.unregister().await;
        }
        // The final status goes out last, once the BPA has released the
        // registration, so a client that re-registers the moment it sees its
        // stream end cannot collide with the registration it just closed.
        if let Err(status) = result {
            let _ = permit.send(Err(status));
        }
        drop(slot);

        #[cfg(test)]
        let _ = self.closed.send(());
    }
}

#[async_trait]
impl ApplicationService for ApplicationServiceImpl {
    type SubscribeStream = EventStream<SubscribeResponse>;

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn subscribe(
        &self,
        request: Request<Streaming<SubscribeRequest>>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let mut requests = request.into_inner();

        #[cfg(test)]
        let _ = self.opened.send(());
        let register = expect_register(
            &mut requests,
            self.tasks.cancel_token(),
            self.limits.handshake,
        )
        .await?;
        let chunk_size = ChunkSize::negotiate(register.max_chunk_size)?;

        // The slot is taken only once the client has asked for a session, so
        // a connection that opens the call and then says nothing cannot hold
        // one for the whole handshake window.
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            warn!(
                "refusing a {LABEL} session: the limit of {} is reached",
                self.limits.max_sessions
            );
            return Err(Status::resource_exhausted("session limit reached"));
        };

        let service_id = register.service_id.map(|id| match id {
            register::ServiceId::Ipn(n) => Service::Ipn(n),
            register::ServiceId::Dtn(demux) => Service::Dtn(demux.into()),
        });

        let (registration_tx, registration_rx) = oneshot::channel();
        let session =
            self.clone()
                .run_session(slot, service_id, chunk_size, requests, registration_tx);
        #[cfg(feature = "instrument")]
        {
            let span = trace_span!(parent: None, "grpc_session", api = LABEL);
            span.follows_from(Span::current());
            self.tasks.spawn(session.instrument(span));
        }
        #[cfg(not(feature = "instrument"))]
        self.tasks.spawn(session);

        let (registration, events_rx) = registration_rx
            .await
            .map_err(|_| Status::unavailable("server shutting down"))??;

        Ok(Response::new(prepend(
            SubscribeResponse {
                event: Some(subscribe_response::Event::Registration(registration)),
            },
            events_rx,
        )))
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
        })) = expect_first(
            &mut requests,
            self.tasks.cancel_token(),
            self.limits.handshake,
            "SendMetadata",
        )
        .await?
        .request
        else {
            return Err(Status::invalid_argument(
                "the first message must be SendMetadata",
            ));
        };

        let application = self.resolve(session_token)?;

        let destination = destination
            .parse::<Eid>()
            .map_err(|_| Status::invalid_argument("SendMetadata.destination is not a valid EID"))?;
        let lifetime = lifetime
            .ok_or_else(|| Status::invalid_argument("SendMetadata.lifetime is required"))
            .and_then(|d| {
                Duration::try_from(d)
                    .map_err(|_| Status::invalid_argument("SendMetadata.lifetime is out of range"))
            })?;

        let options = options.map(SendOptions::from);

        // A call beyond the session's transfer bound waits its turn: it holds
        // only its metadata until then, and the client is the one waiting.
        let _transfer = tokio::select! {
            biased;
            _ = application.cancel.cancelled() => {
                return Err(Status::unavailable("registration closed"));
            }
            permit = application.transfers.acquire() => {
                permit.map_err(|_| Status::unavailable("registration closed"))?
            }
        };
        let mut inner = ChunkReceiver::new(&mut requests, &application.cancel);
        let mut receiver = BoundedChunkReceiver::new(
            &mut inner,
            application.watchdog.limits(),
            application.chunk_size,
            adu_size,
        )?;
        match application
            .registered
            .get()
            .ok_or_else(|| Status::unavailable("registration closed"))?
            .send(destination, lifetime, options, adu_size, &mut receiver)
            .await
        {
            Ok(bundle_id) => Ok(Response::new(SendResponse {
                bundle_id: bundle_id.to_key(),
            })),
            Err(e) => Err(match receiver.into_error() {
                Some(TransferError::Stalled) => application.watchdog.stall(Stage::Feed),
                Some(TransferError::Failed(status)) => status,
                None => inner.into_error().unwrap_or_else(|| service_status(e)),
            }),
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
        })) = expect_first(
            &mut requests,
            self.tasks.cancel_token(),
            self.limits.handshake,
            "ReceiveMetadata",
        )
        .await?
        .request
        else {
            return Err(Status::invalid_argument(
                "the first message must be ReceiveMetadata",
            ));
        };

        let application = self.resolve(session_token)?;
        let bundle_id = BundleId::from_key(&bundle_id).map_err(|_| {
            Status::invalid_argument("ReceiveMetadata.bundle_id is not a valid bundle id")
        })?;
        let Some(responses_rx) = application.deliveries.collect(&bundle_id, requests) else {
            return Err(Status::not_found("no such Delivery"));
        };
        Ok(Response::new(responses_rx))
    }
}

#[cfg(test)]
mod tests {
    use core::{
        future::Future,
        num::NonZeroUsize,
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use std::time::Duration;

    use hardy_async::CancellationToken;
    use hardy_bpa::bpa::Bpa;
    use tonic::{
        Code,
        transport::{Channel, Server},
    };

    use super::*;
    use crate::{
        application::{
            Register, application_service_client::ApplicationServiceClient,
            application_service_server::ApplicationServiceServer, register, subscribe_request,
        },
        server::{
            services::tests::{build_bpa, ipn1, serve, timeout},
            watchdog::Stage,
        },
    };

    fn event() -> subscribe_response::Event {
        subscribe_response::Event::Delivery(Delivery::default())
    }

    #[tokio::test]
    async fn an_event_after_the_session_closes_is_refused() {
        let cancel = CancellationToken::new();
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let application = GrpcApplication::new(
            cancel.clone(),
            Limits::default(),
            ChunkSize::default(),
            events_tx.downgrade(),
        );

        cancel.cancel();

        let Err(status) = application.send_event(event()).await else {
            panic!("no event may follow a close");
        };
        assert_eq!(status.code(), Code::Unavailable);
        assert_eq!(status.message(), "registration closed");
        assert!(events_rx.try_recv().is_err(), "no event may follow a close");
    }

    #[tokio::test]
    async fn an_event_after_the_session_is_retired_is_refused() {
        let (events_tx, _events_rx) = mpsc::channel(1);
        let application = GrpcApplication::new(
            CancellationToken::new(),
            Limits::default(),
            ChunkSize::default(),
            events_tx.downgrade(),
        );

        drop(events_tx);
        let Err(status) = application.send_event(event()).await else {
            panic!("no event may follow a retired session");
        };
        assert_eq!(status.code(), Code::Unavailable);
        assert_eq!(status.message(), "registration closed");
    }

    #[tokio::test]
    async fn an_event_blocked_on_a_full_buffer_is_freed_by_teardown() {
        let cancel = CancellationToken::new();
        let (events_tx, _events_rx) = mpsc::channel(1);
        let application = GrpcApplication::new(
            cancel.clone(),
            Limits::default(),
            ChunkSize::default(),
            events_tx.downgrade(),
        );
        assert!(application.send_event(event()).await.is_ok());

        let mut parked = pin!(application.send_event(event()));
        assert!(
            parked
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the send must park on the full buffer"
        );

        cancel.cancel();
        let Poll::Ready(sent) = parked.poll(&mut Context::from_waker(Waker::noop())) else {
            panic!("teardown alone must free a parked event");
        };
        let Err(status) = sent else {
            panic!("teardown must refuse the parked event");
        };
        assert_eq!(status.code(), Code::Unavailable);
    }

    #[tokio::test]
    async fn an_event_the_client_leaves_no_room_for_stalls_the_session() {
        let (events_tx, _events_rx) = mpsc::channel(1);
        let application = GrpcApplication::new(
            CancellationToken::new(),
            Limits {
                idle: Duration::ZERO,
                ..Limits::default()
            },
            ChunkSize::default(),
            events_tx.downgrade(),
        );

        assert!(application.send_event(event()).await.is_ok());
        let Err(status) = application.send_event(event()).await else {
            panic!("an event the client leaves no room for must stall");
        };
        assert_eq!(status.code(), Code::DeadlineExceeded);
        assert_eq!(
            application.watchdog.stalled().await,
            Stage::Event,
            "the stall must be recorded against the event stage"
        );
    }

    struct Harness {
        bpa: Arc<Bpa>,
        tasks: TaskPool,
        client: ApplicationServiceClient<Channel>,
        server: ApplicationServiceImpl,
    }

    async fn harness() -> Harness {
        harness_with_limits(Limits::default()).await
    }

    async fn harness_with_limits(limits: Limits) -> Harness {
        let bpa = build_bpa(ipn1(), true).await;

        let tasks = TaskPool::new();
        let server = ApplicationServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits);
        let service = ApplicationServiceServer::new(server.clone());
        let address = serve(Server::builder().add_service(service)).await;

        let client = ApplicationServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        Harness {
            bpa,
            tasks,
            client,
            server,
        }
    }

    type Session = (mpsc::Sender<SubscribeRequest>, Streaming<SubscribeResponse>);

    async fn register(
        client: &mut ApplicationServiceClient<Channel>,
        service_id: Option<register::ServiceId>,
    ) -> Session {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id,
                    max_chunk_size: None,
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
            panic!("expected the Registration event first");
        };
        assert!(!registration.session_token.is_empty());

        (requests_tx, events)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unregistered_subscription_does_not_block_shutdown() {
        let harness = harness().await;
        let mut opened = harness.server.opened.subscribe();

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

    fn one_session() -> Limits {
        Limits {
            max_sessions: NonZeroUsize::new(1).unwrap(),
            ..Limits::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_session_returns_its_slot() {
        let mut harness = harness_with_limits(one_session()).await;
        let mut closed = harness.server.closed.subscribe();

        let held = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        drop(held);
        timeout(closed.recv()).await.unwrap();

        let _next = register(&mut harness.client, Some(register::ServiceId::Ipn(8))).await;

        harness.bpa.shutdown().await;
    }
}
