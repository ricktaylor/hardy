//! The `hardy.routing.v1` API.

use std::sync::Arc;

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::{CancellationToken, TaskPool, sync::spin::Once};
use hardy_bpa::{
    Bytes, async_trait,
    bpa::BpaRegistration,
    routing::{self, RoutingAgent, RoutingSink},
};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use tokio::sync::{
    OwnedSemaphorePermit, Semaphore,
    mpsc::{Receiver, channel},
    oneshot,
};
use tonic::{Request, Response, Status, Streaming};
use tracing::warn;
#[cfg(feature = "instrument")]
use tracing::{Instrument, Span, instrument, trace_span};

use super::{EventStream, expect_register, prepend, wait_for_unregister};
use crate::{
    MAX_MESSAGE_SIZE, MAX_TRANSFER_SIZE,
    chunking::DEFAULT_CHUNK_SIZE,
    common::Sizes,
    routing::{
        AddRouteRequest, AddRouteResponse, Registration, RemoveRouteRequest, RemoveRouteResponse,
        RouteActionError, SubscribeRequest, SubscribeResponse,
        routing_service_server::{RoutingService, RoutingServiceServer},
        subscribe_response,
    },
    server::{Limits, status::routing_status},
    token::Token,
};

/// The API's name, used in token subjects, logs, and spans.
const LABEL: &str = "routing";

/// The number of events a session stream buffers ahead of the client; one more
/// slot is reserved for the status the session ends with. A routing session
/// carries no events after its registration, so the depth only has to hold the
/// ending.
const EVENT_DEPTH: usize = 2;

impl From<RouteActionError> for Status {
    fn from(e: RouteActionError) -> Self {
        Status::invalid_argument(match e {
            RouteActionError::InvalidVia(_) => "RouteAction.via is not a valid EID",
            RouteActionError::ReservedReason => "RouteAction.drop uses the reserved reason code",
        })
    }
}

/// What a session hands back once it has registered: the `Registration` event
/// and the receiving end of its event channel.
type Session = (Registration, Receiver<Result<SubscribeResponse, Status>>);

/// The server side of one routing agent session: the [`RoutingAgent`] the BPA
/// holds, and the handler a route call resolves its token to.
///
/// The BPA calls a routing agent back only to register and unregister it;
/// `on_unregister` ends the session. The route calls drive the sink the BPA
/// handed over at registration.
struct GrpcRoutingAgent {
    cancel: CancellationToken,
    registered: Once<Box<dyn RoutingSink>>,
}

#[async_trait]
impl RoutingAgent for GrpcRoutingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        self.registered.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        self.cancel.cancel();
    }
}

/// The `hardy.routing.v1` API of a BPA.
///
/// Implements the generated tonic trait [`RoutingService`] over a
/// [`BpaRegistration`]. Each `Subscribe` registers a proxy [`RoutingAgent`]
/// with the BPA and runs a session task on the API's [`TaskPool`]; `AddRoute`
/// and `RemoveRoute` present the session token and maintain the agent's routes
/// in the BPA's routing information base, which the BPA removes when the
/// session ends. A host wraps the API in
/// [`RoutingServiceServer`](crate::routing::routing_service_server::RoutingServiceServer)
/// and adds it to a tonic server; the [`server`](crate::server) module shows
/// how.
///
/// Clones share one session index and one session ceiling, so an API may be
/// cloned freely. Shutting the pool down ends every session and the host should
/// do so when the tonic server stops.
#[derive(Clone)]
pub struct RoutingServiceImpl {
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    limits: Limits,
    slots: Arc<Semaphore>,
    sessions: Arc<DashMap<Token, Arc<GrpcRoutingAgent>, RandomState>>,
}

impl RoutingServiceImpl {
    /// Creates the API over `bpa` with the default [`Limits`], running its sessions
    /// on `tasks`.
    ///
    /// Of the limits, only the handshake and the session ceiling apply to a routing
    /// session, which has no transfers and no events.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self::with_limits(bpa, tasks, Limits::default())
    }

    /// Creates the API over `bpa` with `limits`, running its sessions on `tasks`.
    ///
    /// Of the limits, only [`handshake`](Limits::handshake) and
    /// [`max_sessions`](Limits::max_sessions) apply to a routing session, which has
    /// no transfers and no events.
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

    /// Wraps the API in its generated tonic server, sized for the wire
    /// contract.
    ///
    /// The server accepts and sends messages of up to [`MAX_MESSAGE_SIZE`]
    /// bytes, as the client SDK does, so a chunk of any size a session can
    /// negotiate fits. A host that mounts the API another way must apply the
    /// same bounds.
    pub fn into_server(self) -> RoutingServiceServer<Self> {
        RoutingServiceServer::new(self)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
    }

    /// Resolves a session token to its live handler.
    ///
    /// # Errors
    ///
    /// Returns `UNAUTHENTICATED` if no live session holds `token`.
    fn resolve(&self, token: Bytes) -> Result<Arc<GrpcRoutingAgent>, Status> {
        self.sessions
            .get(&Token::from(token))
            .map(|agent| agent.clone())
            .ok_or_else(|| Status::unauthenticated("unknown session token"))
    }

    /// Runs one session from registration to its end.
    ///
    /// Registers the handler with the BPA under `name`, indexes its token, hands
    /// the `Registration` and the event channel back through `registration_tx`, and
    /// then waits for whichever comes first: the client dropping the response
    /// stream, server shutdown, the BPA unregistering the handler, or the client's
    /// `Unregister` or half-close. The exit sequence is then fixed: cancel the
    /// session's token, retire it from the index, and unregister the sink from the
    /// BPA, which removes the agent's routes. Only then is an ending the client did
    /// not ask for written as the stream's final status, into the slot reserved on
    /// the event channel so that a full buffer cannot swallow it. By the time the
    /// client sees the stream end the token is dead and the name is free again.
    async fn run_session(
        self,
        _slot: OwnedSemaphorePermit,
        name: String,
        mut requests: Streaming<SubscribeRequest>,
        registration_tx: oneshot::Sender<Result<Session, Status>>,
    ) {
        let (events_tx, events_rx) = channel(EVENT_DEPTH + 1);
        let permit = events_tx
            .clone()
            .try_reserve_owned()
            .expect("a freshly created channel has spare capacity");
        let token = Token::mint(&format!("{LABEL}:{name}"));
        let child = self.tasks.child_token();
        let agent = Arc::new(GrpcRoutingAgent {
            cancel: child.clone(),
            registered: Once::new(),
        });

        let node_ids = match self.bpa.register_routing_agent(name, agent.clone()).await {
            Ok(node_ids) => node_ids,
            Err(e) => {
                let _ = registration_tx.send(Err(routing_status(e)));
                return;
            }
        };

        self.sessions.insert(token.clone(), agent.clone());

        let registration = Registration {
            node_ids: node_ids.iter().map(ToString::to_string).collect(),
            session_token: token.clone().into(),
            sizes: Some(Sizes {
                max_message_size: MAX_MESSAGE_SIZE as u64,
                // The routing API has no transfers, so it announces the server's own chunk size.
                chunk_size: DEFAULT_CHUNK_SIZE as u64,
                max_transfer_size: MAX_TRANSFER_SIZE,
            }),
        };
        let result = if registration_tx.send(Ok((registration, events_rx))).is_ok() {
            tokio::select! {
                biased;
                _ = events_tx.closed() => Ok(()),
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
        if let Some(sink) = agent.registered.get() {
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

#[async_trait]
impl RoutingService for RoutingServiceImpl {
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

        let (registration_tx, registration_rx) = oneshot::channel();
        let session = self
            .clone()
            .run_session(slot, register.name, requests, registration_tx);
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
    async fn add_route(
        &self,
        request: Request<AddRouteRequest>,
    ) -> Result<Response<AddRouteResponse>, Status> {
        let AddRouteRequest {
            session_token,
            pattern,
            action,
            priority,
        } = request.into_inner();
        let agent = self.resolve(session_token)?;
        let pattern = pattern.parse::<EidPattern>().map_err(|_| {
            Status::invalid_argument("AddRouteRequest.pattern is not a valid EID pattern")
        })?;
        let action: routing::RouteAction = action
            .and_then(|a| a.action)
            .ok_or_else(|| Status::invalid_argument("AddRouteRequest.action is required"))?
            .try_into()?;

        let added = agent
            .registered
            .get()
            .ok_or_else(|| Status::unavailable("registration closed"))?
            .add_route(pattern, action, priority)
            .await
            .map_err(routing_status)?;
        Ok(Response::new(AddRouteResponse { added }))
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn remove_route(
        &self,
        request: Request<RemoveRouteRequest>,
    ) -> Result<Response<RemoveRouteResponse>, Status> {
        let RemoveRouteRequest {
            session_token,
            pattern,
            action,
            priority,
        } = request.into_inner();
        let agent = self.resolve(session_token)?;
        let pattern = pattern.parse::<EidPattern>().map_err(|_| {
            Status::invalid_argument("RemoveRouteRequest.pattern is not a valid EID pattern")
        })?;
        let action: routing::RouteAction = action
            .and_then(|a| a.action)
            .ok_or_else(|| Status::invalid_argument("RemoveRouteRequest.action is required"))?
            .try_into()?;

        let removed = agent
            .registered
            .get()
            .ok_or_else(|| Status::unavailable("registration closed"))?
            .remove_route(&pattern, &action, priority)
            .await
            .map_err(routing_status)?;
        Ok(Response::new(RemoveRouteResponse { removed }))
    }
}
