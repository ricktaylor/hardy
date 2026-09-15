//! The `hardy.service.v1` API.

use core::fmt;
use std::sync::Arc;

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::{CancellationToken, TaskPool, sync::spin::Once};
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
use tokio::sync::{
    OwnedSemaphorePermit, Semaphore,
    mpsc::{self, WeakSender, channel},
    oneshot,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::{Instrument, Span, instrument, trace_span};
use tracing::{debug, warn};

use super::deliver;
use crate::{
    chunking::{BoundedChunkReceiver, MAX_DECLARED_TRANSFER_SIZE, MaxChunkSize},
    common::Sizes,
    server::{
        Limits, MAX_INBOUND_TRANSFERS, MAX_SERVER_MESSAGE_SIZE,
        announce::{AnnounceError, Announcements},
        services::{
            EventStream, dtn_service_id, expect_first, expect_register, prepend, send_event,
            token_subject, transfer_slot, wait_for_unregister,
        },
        status::{internal, service_status},
    },
    service::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse,
        Registration, SendMetadata, SendRequest, SendResponse, StatusAssertion, SubscribeRequest,
        SubscribeResponse, receive_request, register, send_request,
        service_service_server::{ServiceService, ServiceServiceServer},
        subscribe_response,
    },
    timeouts::{SLOW_CONSUMER, Timeouts},
    timestamp::to_timestamp,
    token::Token,
};

/// The API's name, used in token subjects, logs, and spans.
const LABEL: &str = "service";

/// The number of events a session stream buffers ahead of the client; one more
/// slot is reserved for the status the session ends with.
const EVENT_DEPTH: usize = 16;

/// What a session hands back once it has registered: the `Registration` event
/// and the receiving end of its event channel.
type Session = (
    Registration,
    mpsc::Receiver<Result<SubscribeResponse, Status>>,
);

/// The server side of one service session: the [`services::Service`] the BPA
/// holds, and the handler a data-plane call resolves its token to.
///
/// The BPA's callbacks become wire exchanges. `on_deliver` announces the
/// bundle, waits for the client's `Receive`, streams the bundle to it, and
/// waits for its `ack`; `on_status_notify` pushes a status report event;
/// `on_unregister` ends the session. The handler keeps only a weak sender for
/// the event stream, so a call that resolved the handler cannot hold the stream
/// open past the session task.
struct GrpcService {
    timeouts: Arc<Timeouts>,
    max_chunk_size: MaxChunkSize,
    registered: Once<Box<dyn ServiceSink>>,
    events_tx: WeakSender<Result<SubscribeResponse, Status>>,
    deliveries: Announcements<ReceiveResponse, ReceiveRequest>,
    /// The `Send` calls the session may have open at once.
    transfers: Semaphore,
}

impl GrpcService {
    /// Creates the handler of a session that `cancel` ends, `limits` bounds,
    /// `events_tx` feeds, and whose transfers chunk at `max_chunk_size`.
    fn new(
        cancel: CancellationToken,
        limits: Limits,
        max_chunk_size: MaxChunkSize,
        events_tx: WeakSender<Result<SubscribeResponse, Status>>,
    ) -> Self {
        let timeouts = Arc::new(Timeouts::new(limits, cancel));
        Self {
            deliveries: Announcements::default(),
            registered: Once::new(),
            events_tx,
            timeouts,
            max_chunk_size,
            transfers: Semaphore::new(MAX_INBOUND_TRANSFERS),
        }
    }

    /// Pushes one event onto the session stream.
    async fn send_event(&self, event: subscribe_response::Event) -> Result<(), Status> {
        send_event(
            &self.events_tx,
            &self.timeouts,
            SubscribeResponse { event: Some(event) },
        )
        .await
    }
}

#[async_trait]
impl services::Service for GrpcService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        self.registered.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        self.timeouts.cancel_token().cancel();
    }

    /// Announces the bundle, then hands it to [`deliver`], which streams it to
    /// the client's `Receive` call and waits for the client to commit it with
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
        bundle_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let delivery = subscribe_response::Event::Delivery(Delivery {
            bundle_id: bundle_id.to_key(),
            expire_time: Some(to_timestamp(expiry)),
            bundle_size,
        });
        let collection = self
            .deliveries
            .announce(&self.timeouts, bundle_id, async move || {
                self.send_event(delivery).await
            })
            .await
            .map_err(|error| match error {
                AnnounceError::AlreadyAnnounced => {
                    warn!("refusing duplicate announcement: bundle already being delivered");
                    services::Error::StreamCancelled
                }
                AnnounceError::Uncollected => services::Error::Disconnected,
            })?;

        deliver(collection, stream, &self.timeouts, self.max_chunk_size).await
    }

    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        from: &Eid,
        kind: services::StatusNotify,
        reason: status_report::ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        let sent = self
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
        // A report is informational, so one the session cannot take is
        // dropped; the stall or the closing it ran into ends the session on
        // its own.
        if let Err(status) = sent {
            debug!("dropping a status report the session cannot take: {status}");
        }
    }
}

/// The `hardy.service.v1` API of a BPA.
///
/// Implements the generated tonic trait [`ServiceService`] over a
/// [`BpaRegistration`]. Each `Subscribe` registers a proxy
/// [`services::Service`] with the BPA and runs a session task on the API's
/// [`TaskPool`]; `Send` and `Receive` present the session token and move
/// complete bundles. A host wraps the API in
/// [`ServiceServiceServer`](crate::service::service_service_server::ServiceServiceServer)
/// and adds it to a tonic server; the [`server`](crate::server) module shows
/// how.
///
/// Clones share one session index and one session ceiling, so an API may be
/// cloned freely. Shutting the pool down ends every session and the host should
/// do so when the tonic server stops.
#[derive(Clone)]
pub struct ServiceServiceImpl {
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    limits: Limits,
    slots: Arc<Semaphore>,
    sessions: Arc<DashMap<Token, Arc<GrpcService>, RandomState>>,
}

impl ServiceServiceImpl {
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
        }
    }

    /// Registers `service` with the BPA under `service_id`, or under a service
    /// number of the BPA's choosing if `None`, and returns the endpoint id.
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

    /// Wraps the API in its generated tonic server, sized for the wire
    /// contract.
    ///
    /// The server accepts and sends messages of up to
    /// [`MAX_SERVER_MESSAGE_SIZE`] bytes, which is a chunk of any size a
    /// session can negotiate plus room for the rest of its message, and is
    /// what the `Registration` announces. A host that mounts the API another
    /// way must apply the same bounds, or a client can make it hold more per
    /// call than [`SESSION_FOOTPRINT`](crate::server::SESSION_FOOTPRINT)
    /// accounts for.
    pub fn into_server(self) -> ServiceServiceServer<Self> {
        ServiceServiceServer::new(self)
            .max_encoding_message_size(MAX_SERVER_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_SERVER_MESSAGE_SIZE)
    }

    /// Resolves a session token to its live handler.
    ///
    /// # Errors
    ///
    /// Returns `UNAUTHENTICATED` if no live session holds `token`.
    fn resolve(&self, token: Bytes) -> Result<Arc<GrpcService>, Status> {
        Token::presented(token)
            .and_then(|token| self.sessions.get(&token).map(|service| service.clone()))
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
    /// full buffer cannot swallow it. By the time the client sees the stream end
    /// the token is dead and the endpoint id is free again.
    async fn run_session(
        self,
        _slot: OwnedSemaphorePermit,
        service_id: Option<Service>,
        max_chunk_size: MaxChunkSize,
        mut requests: Streaming<SubscribeRequest>,
        registration_tx: oneshot::Sender<Result<Session, Status>>,
    ) {
        let (events_tx, events_rx) = channel(EVENT_DEPTH + 1);
        let permit = events_tx
            .clone()
            .try_reserve_owned()
            .expect("a freshly created channel has spare capacity");
        let token = match Token::mint(&token_subject(LABEL, service_id.as_ref())) {
            Ok(token) => token,
            Err(error) => {
                let _ = registration_tx.send(Err(internal(error)));
                return;
            }
        };
        let child = self.tasks.child_token();
        let service = Arc::new(GrpcService::new(
            child.clone(),
            self.limits,
            max_chunk_size,
            events_tx.downgrade(),
        ));

        let endpoint_id = match self.register(service_id, service.clone()).await {
            Ok(endpoint_id) => endpoint_id,
            Err(error) => {
                let _ = registration_tx.send(Err(service_status(error)));
                return;
            }
        };

        self.sessions.insert(token.clone(), service.clone());

        let registration = Registration {
            endpoint_id: endpoint_id.to_string(),
            session_token: token.clone().into(),
            sizes: Some(Sizes {
                max_message_size: MAX_SERVER_MESSAGE_SIZE as u64,
                max_chunk_size: max_chunk_size.get() as u64,
                max_transfer_size: MAX_DECLARED_TRANSFER_SIZE,
            }),
        };
        let result = if registration_tx.send(Ok((registration, events_rx))).is_ok() {
            tokio::select! {
                biased;
                _ = events_tx.closed() => Ok(()),
                _ = service.timeouts.timed_out() => Err(Status::deadline_exceeded(SLOW_CONSUMER)),
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
        if let Some(sink) = service.registered.get() {
            sink.unregister().await;
        }
        // The final status goes out last, once the BPA has released the
        // registration, so a client that re-registers the moment it sees its
        // stream end cannot collide with the registration it just closed.
        if let Err(status) = result {
            let _ = permit.send(Err(status));
        }
    }
}

/// Prints the API's bounds and how many sessions it is running, never a
/// session's token.
impl fmt::Debug for ServiceServiceImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceServiceImpl")
            .field("limits", &self.limits)
            .field("sessions", &self.sessions.len())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ServiceService for ServiceServiceImpl {
    type SubscribeStream = EventStream<SubscribeResponse>;

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn subscribe(
        &self,
        request: Request<Streaming<SubscribeRequest>>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let mut requests = request.into_inner();

        let register = expect_register(
            &mut requests,
            self.tasks.cancel_token(),
            self.limits.handshake,
        )
        .await?;
        let max_chunk_size = MaxChunkSize::negotiate(register.max_chunk_size)?;

        // The slot is taken only once the client has asked for a session, so
        // a connection that opens the call and then says nothing cannot hold
        // one for the whole handshake window.
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            // Nothing has authenticated yet, so this is not worth more than a
            // debug line: a peer can trigger it as often as it likes.
            debug!(
                "refusing a {LABEL} session: the limit of {} is reached",
                self.limits.max_sessions
            );
            return Err(Status::resource_exhausted("session limit reached"));
        };

        let service_id = register
            .service_id
            .map(|id| match id {
                register::ServiceId::Ipn(n) => Ok(Service::Ipn(n)),
                register::ServiceId::Dtn(demux) => dtn_service_id(demux),
            })
            .transpose()?;

        let (registration_tx, registration_rx) = oneshot::channel();
        let session =
            self.clone()
                .run_session(slot, service_id, max_chunk_size, requests, registration_tx);
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
            bundle_size,
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

        let service = self.resolve(session_token)?;
        let _transfer = transfer_slot(&service.transfers, &service.timeouts).await?;
        let mut receiver = BoundedChunkReceiver::new(
            &mut requests,
            &service.timeouts,
            service.max_chunk_size,
            bundle_size,
        )?;
        match service
            .registered
            .get()
            .ok_or_else(|| Status::unavailable("registration closed"))?
            .send(&mut receiver)
            .await
        {
            Ok(bundle_id) => Ok(Response::new(SendResponse {
                bundle_id: bundle_id.to_key(),
            })),
            Err(error) => Err(receiver.into_error().unwrap_or_else(|| match error {
                // The receiver recorded no ending of its own, so the stream
                // was ended from the BPA's side: the registration is being
                // torn down, which cancels its streams before it tells this
                // session.
                services::Error::StreamCancelled => Status::unavailable("registration closed"),
                error => service_status(error),
            })),
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

        let service = self.resolve(session_token)?;
        let bundle_id = BundleId::from_key(&bundle_id).map_err(|_| {
            Status::invalid_argument("ReceiveMetadata.bundle_id is not a valid bundle id")
        })?;
        let Some(responses_rx) = service.deliveries.collect(&bundle_id, requests) else {
            return Err(Status::not_found("no such Delivery"));
        };
        Ok(Response::new(responses_rx))
    }
}
