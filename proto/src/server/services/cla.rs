//! The `hardy.cla.v1` API.

use core::{
    fmt,
    future::Future,
    num::{NonZeroU32, NonZeroU64},
};
use std::sync::Arc;

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::{CancellationToken, TaskPool, sync::spin::Once};
// The BPA's address type and error are aliased: the wire `ClaAddressType` is
// the one this file converts, and `Error` is the BPA's error to the wire's
// `Status`.
use hardy_bpa::{
    Bytes, async_trait,
    bpa::BpaRegistration,
    cla::{
        Cla, ClaAddress, ClaAddressType as DomainClaAddressType, ClaInit, Error as ClaError,
        ForwardBundleResult, Sink, TransferOutcome,
    },
    stream::{Receiver, Segment},
};
use hardy_bpv7::{bundle::Id as BundleId, eid::NodeId};
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

use super::{
    EventStream, expect_first, expect_register, is_disconnect, prepend, registered_name,
    send_event, transfer_slot, wait_for_unregister,
};
use crate::{
    MAX_LANE_COUNT,
    chunking::{
        BoundedChunkReceiver, BoundedChunkSender, MAX_DECLARED_TRANSFER_SIZE, MaxChunkSize,
    },
    cla::{
        Acceptance, AddPeerRequest, AddPeerResponse, ClaAddressType, DispatchMetadata,
        DispatchRequest, DispatchResponse, ForwardMetadata, ForwardRequest, ForwardResponse,
        Forwarding, Registration, RemovePeerRequest, RemovePeerResponse,
        ReportTransferOutcomeRequest, ReportTransferOutcomeResponse, SubscribeRequest,
        SubscribeResponse,
        cla_service_server::{ClaService, ClaServiceServer},
        dispatch_request, forward_request, forward_result, report_transfer_outcome_request,
        subscribe_response,
    },
    common::Sizes,
    server::{
        Limits, MAX_INBOUND_TRANSFERS, MAX_OUTBOUND_FORWARDINGS, MAX_SERVER_MESSAGE_SIZE,
        announce::{AnnounceError, Announcements, Collection},
        status::{cla_status, internal},
    },
    timeouts::{SLOW_CONSUMER, Stage, Timeouts},
    token::Token,
};

/// The API's name, used in token subjects, logs, and spans.
const LABEL: &str = "cla";

/// The number of events a session stream buffers ahead of the client; one more
/// slot is reserved for the status the session ends with.
const EVENT_DEPTH: usize = 16;

/// What a session hands back once it has registered: the `Registration` event
/// and the receiving end of its event channel.
type Session = (
    Registration,
    mpsc::Receiver<Result<SubscribeResponse, Status>>,
);

/// What the BPA hands a CLA at registration: its sink, and the bundle size
/// limit the BPA agreed to.
struct Registered {
    sink: Box<dyn Sink>,
    max_bundle_size: Option<NonZeroU64>,
}

/// The server side of one CLA session: the [`Cla`] the BPA holds, and the
/// handler a data-plane call resolves its token to.
///
/// The BPA's `forward` becomes a wire exchange: it announces the bundle, waits
/// for the client's `Forward`, streams the bundle to it, and waits for its
/// result. `on_unregister` ends the session. The handler keeps only a weak
/// sender for the event stream, so a call that resolved the handler cannot hold
/// the stream open past the session task.
struct GrpcCla {
    timeouts: Arc<Timeouts>,
    max_chunk_size: MaxChunkSize,
    registered: Once<Registered>,
    events_tx: WeakSender<Result<SubscribeResponse, Status>>,
    forwardings: Announcements<ForwardResponse, ForwardRequest>,
    /// The `Dispatch` calls the session may have open at once.
    transfers: Semaphore,
    /// The forwardings the session may have in flight at once.
    forwards: Semaphore,
}

impl GrpcCla {
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
            forwardings: Announcements::default(),
            registered: Once::new(),
            events_tx,
            timeouts,
            max_chunk_size,
            transfers: Semaphore::new(MAX_INBOUND_TRANSFERS),
            forwards: Semaphore::new(MAX_OUTBOUND_FORWARDINGS),
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

    /// Drives `call` on the sink unless the session ends first.
    ///
    /// A call that resolved its token just before the session was torn down
    /// would otherwise reach the BPA after the registration it belongs to has
    /// started to unwind, and the state it adds then is never swept.
    ///
    /// # Errors
    ///
    /// Returns `UNAVAILABLE` if the session ends before `call` completes.
    async fn alive<T>(&self, call: impl Future<Output = T>) -> Result<T, Status> {
        tokio::select! {
            biased;
            _ = self.timeouts.cancelled() => Err(Status::unavailable("registration closed")),
            value = call => Ok(value),
        }
    }
}

/// What the client sent to end a forwarding.
///
/// A `cancel` is the client asking for the ending, not a fault, so the call it
/// arrives on ends `OK` and nothing is sent back.
enum ForwardCompletion {
    /// The client reported the transfer's result.
    Reported(ForwardBundleResult),
    /// The client sent `cancel`.
    Cancelled,
}

/// Waits for the CLA's `ForwardResult` or `cancel` on the request side of a
/// `Forward` call.
///
/// Any other message is ignored; the first one is logged.
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` on a result with nothing set, `CANCELLED` if the
/// stream ends first, and `ABORTED` if it fails.
async fn wait_for_forward_completion(
    requests: &mut Streaming<ForwardRequest>,
) -> Result<ForwardCompletion, Status> {
    let mut warned = false;
    loop {
        match requests.message().await {
            Ok(Some(ForwardRequest {
                request: Some(forward_request::Request::Result(result)),
            })) => {
                let result = match result.result {
                    Some(forward_result::Result::Sent(_)) => ForwardBundleResult::Sent,
                    Some(forward_result::Result::NoNeighbour(_)) => {
                        ForwardBundleResult::NoNeighbour
                    }
                    Some(forward_result::Result::Accepted(_)) => ForwardBundleResult::Accepted,
                    None => {
                        return Err(Status::invalid_argument("ForwardResult.result is required"));
                    }
                };
                return Ok(ForwardCompletion::Reported(result));
            }
            Ok(Some(ForwardRequest {
                request: Some(forward_request::Request::Cancel(_)),
            })) => return Ok(ForwardCompletion::Cancelled),
            Ok(Some(_)) if !warned => {
                warned = true;
                warn!("ignoring unexpected message on the Forward request stream");
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(Status::cancelled("request stream closed before the result"));
            }
            Err(error) => {
                debug!("request stream failed: {error}");
                return Err(Status::aborted("request stream failed"));
            }
        }
    }
}

#[async_trait]
impl Cla for GrpcCla {
    async fn on_register(
        &self,
        sink: Box<dyn Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        self.registered.call_once(|| Registered {
            sink,
            max_bundle_size,
        });
    }

    async fn on_unregister(&self) {
        self.timeouts.cancel_token().cancel();
    }

    /// Announces the bundle, streams it to the client's `Forward` call, and
    /// returns the result the CLA reports.
    ///
    /// The result is awaited from the first chunk, since a neighbour can go
    /// away at any point, and once the client has taken the last chunk it owes
    /// the result within the idle bound, so a CLA whose transmission may
    /// outlast that owes `accepted` and a later `ReportTransferOutcome` rather
    /// than a late `sent`. Every failure is told to the client, through the slot the
    /// collection holds for its ending, and becomes the BPA's error so that the
    /// bundle is kept: a stall or a closed session as `Disconnected`, anything
    /// else as `StreamCancelled`.
    async fn forward(
        &self,
        lane: Option<u32>,
        cla_addr: &ClaAddress,
        bundle_id: &BundleId,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<ForwardBundleResult, ClaError> {
        // A forwarding beyond the session's bound waits its turn, so what the
        // BPA's fan-out over a client's peers can commit the server to is
        // bounded by the session rather than by the peers the client added.
        let _forward = tokio::select! {
            biased;
            _ = self.timeouts.cancelled() => return Err(ClaError::Disconnected),
            permit = self.forwards.acquire() => {
                permit.map_err(|_| ClaError::Disconnected)?
            }
        };

        let forwarding = subscribe_response::Event::Forwarding(Forwarding {
            bundle_id: bundle_id.to_key(),
            address: Some(cla_addr.clone().into()),
            lane,
            bundle_size: total_len,
        });
        let Collection {
            mut requests,
            responses_tx,
            guard,
        } = self
            .forwardings
            .announce(&self.timeouts, bundle_id, async move || {
                self.send_event(forwarding).await
            })
            .await
            .map_err(|error| match error {
                AnnounceError::AlreadyAnnounced => {
                    warn!("refusing duplicate announcement: bundle already being forwarded");
                    ClaError::StreamCancelled
                }
                AnnounceError::Uncollected => ClaError::Disconnected,
            })?;

        let mut sender =
            BoundedChunkSender::new(responses_tx, &self.timeouts, stream, self.max_chunk_size);
        // The send is polled before the request side, because the client's
        // result and the send's own completion become ready together: the send
        // ends by waiting for the client to take the last chunk, which is what
        // the client reports on. Reading the request side first would charge a
        // prompt client with a result that in fact came after the chunk it
        // reports.
        let transfer = tokio::select! {
            biased;
            _ = self.timeouts.cancelled() => Err(Status::unavailable("registration closed")),
            sent = sender.send_all() => sent,
            result = wait_for_forward_completion(&mut requests) => match result {
                // A neighbour can go away at any point, so this one result may
                // arrive before the bundle does. The transfer is abandoned
                // where it stands, and the BPA re-reads the bundle to route it
                // elsewhere.
                Ok(ForwardCompletion::Reported(ForwardBundleResult::NoNeighbour)) => {
                    drop(guard.disarm());
                    return Ok(ForwardBundleResult::NoNeighbour);
                }
                Ok(ForwardCompletion::Reported(
                    ForwardBundleResult::Sent | ForwardBundleResult::Accepted,
                )) => Err(Status::invalid_argument("result before the last chunk")),
                // A cancel is the client asking for the ending, so the slot
                // is released unused and the stream closes `OK`.
                Ok(ForwardCompletion::Cancelled) => {
                    drop(guard.disarm());
                    return Err(ClaError::StreamCancelled);
                }
                Err(status) => Err(status),
            },
        };

        let status = match transfer {
            Err(status) => status,
            Ok(()) => {
                let completion = self
                    .timeouts
                    .bound(Stage::Ack, wait_for_forward_completion(&mut requests))
                    .await;
                match completion {
                    Ok(ForwardCompletion::Reported(result)) => {
                        drop(guard.disarm());
                        return Ok(result);
                    }
                    Ok(ForwardCompletion::Cancelled) => {
                        drop(guard.disarm());
                        return Err(ClaError::StreamCancelled);
                    }
                    Err(status) => status,
                }
            }
        };

        // The client is told how the call ends, through the slot the collection
        // holds for its ending.
        let error = if is_disconnect(&status) {
            ClaError::Disconnected
        } else {
            ClaError::StreamCancelled
        };
        guard.disarm().send(Err(status));
        Err(error)
    }
}

/// The `hardy.cla.v1` API of a BPA.
///
/// Implements the generated tonic trait [`ClaService`] over a
/// [`BpaRegistration`]. Each `Subscribe` registers a proxy [`Cla`] with the BPA
/// and runs a session task on the API's [`TaskPool`]; `Dispatch` and `Forward`
/// present the session token and move bundles, and `AddPeer`, `RemovePeer` and
/// `ReportTransferOutcome` drive the CLA's sink. A host wraps the API in
/// [`ClaServiceServer`](crate::cla::cla_service_server::ClaServiceServer) and
/// adds it to a tonic server; the [`server`](crate::server) module shows how.
///
/// Clones share one session index and one session ceiling, so an API may be
/// cloned freely. Shutting the pool down ends every session and the host should
/// do so when the tonic server stops.
#[derive(Clone)]
pub struct ClaServiceImpl {
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    limits: Limits,
    slots: Arc<Semaphore>,
    sessions: Arc<DashMap<Token, Arc<GrpcCla>, RandomState>>,
}

impl ClaServiceImpl {
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
    pub fn into_server(self) -> ClaServiceServer<Self> {
        ClaServiceServer::new(self)
            .max_encoding_message_size(MAX_SERVER_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_SERVER_MESSAGE_SIZE)
    }

    /// Resolves a session token to its live handler.
    ///
    /// # Errors
    ///
    /// Returns `UNAUTHENTICATED` if no live session holds `token`.
    fn resolve(&self, token: Bytes) -> Result<Arc<GrpcCla>, Status> {
        Token::presented(token)
            .and_then(|token| self.sessions.get(&token).map(|cla| cla.clone()))
            .ok_or_else(|| Status::unauthenticated("unknown session token"))
    }

    /// Runs one session from registration to its end.
    ///
    /// Registers the handler with the BPA under `name` and `init`, indexes its
    /// token, hands the `Registration`, which carries the bundle size limit the BPA
    /// agreed to, and the event channel back through `registration_tx`, and then
    /// waits for whichever comes first: the client dropping the response stream, a
    /// stall, server shutdown, the BPA unregistering the handler, or the client's
    /// `Unregister` or half-close. The exit sequence is then fixed: cancel the
    /// session's token, retire it from the index, and unregister the sink from the
    /// BPA. Only then is an ending the client did not ask for written as the
    /// stream's final status, into the slot reserved on the event channel so that a
    /// full buffer cannot swallow it. By the time the client sees the stream end
    /// the token is dead and the name is free again.
    async fn run_session(
        self,
        _slot: OwnedSemaphorePermit,
        name: String,
        init: ClaInit,
        max_chunk_size: MaxChunkSize,
        mut requests: Streaming<SubscribeRequest>,
        registration_tx: oneshot::Sender<Result<Session, Status>>,
    ) {
        let (events_tx, events_rx) = channel(EVENT_DEPTH + 1);
        let permit = events_tx
            .clone()
            .try_reserve_owned()
            .expect("a freshly created channel has spare capacity");
        let token = match Token::mint(&format!("{LABEL}:{name}")) {
            Ok(token) => token,
            Err(error) => {
                let _ = registration_tx.send(Err(internal(error)));
                return;
            }
        };
        let child = self.tasks.child_token();
        let cla = Arc::new(GrpcCla::new(
            child.clone(),
            self.limits,
            max_chunk_size,
            events_tx.downgrade(),
        ));

        let node_ids = match self.bpa.register_cla(name, cla.clone(), None, init).await {
            Ok(node_ids) => node_ids,
            Err(error) => {
                let _ = registration_tx.send(Err(cla_status(error)));
                return;
            }
        };

        // `BpaRegistration::register_cla` must drive `on_register` to
        // completion before it returns. One that does not has left nothing to
        // serve the session with, and no sink to give the registration back
        // through either.
        let Some(registered) = cla.registered.get() else {
            // The BPA holds a registration this session cannot drive or end,
            // so at least every call on it fails fast.
            child.cancel();
            let _ = registration_tx.send(Err(internal(format!(
                "the BPA registered the {LABEL} without delivering its sink"
            ))));
            return;
        };
        let max_bundle_size = registered.max_bundle_size;

        self.sessions.insert(token.clone(), cla.clone());

        let registration = Registration {
            node_ids: node_ids.iter().map(ToString::to_string).collect(),
            session_token: token.clone().into(),
            max_bundle_size: max_bundle_size.map(NonZeroU64::get),
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
                _ = cla.timeouts.timed_out() => Err(Status::deadline_exceeded(SLOW_CONSUMER)),
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
        if let Some(registered) = cla.registered.get() {
            registered.sink.unregister().await;
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
impl fmt::Debug for ClaServiceImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaServiceImpl")
            .field("limits", &self.limits)
            .field("sessions", &self.sessions.len())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ClaService for ClaServiceImpl {
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
        let name = registered_name("Register.name", register.name)?;

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

        let address_type = register
            .address_type
            .map(|address_type| {
                ClaAddressType::try_from(address_type)
                    .ok()
                    .and_then(|address_type| DomainClaAddressType::try_from(address_type).ok())
                    .ok_or_else(|| {
                        Status::invalid_argument("Register.address_type is unspecified or unknown")
                    })
            })
            .transpose()?;

        let lane_count = match register.lane_count {
            Some(0) => {
                return Err(Status::invalid_argument(
                    "Register.lane_count must not be zero",
                ));
            }
            Some(n) if n > MAX_LANE_COUNT => {
                return Err(Status::invalid_argument(format!(
                    "Register.lane_count exceeds the maximum of {MAX_LANE_COUNT}"
                )));
            }
            Some(n) => NonZeroU32::new(n),
            None => None,
        };
        let max_bundle_size = match register.max_bundle_size {
            Some(0) => {
                return Err(Status::invalid_argument(
                    "Register.max_bundle_size must not be zero",
                ));
            }
            Some(n) => NonZeroU64::new(n),
            None => None,
        };
        let init = ClaInit {
            address_type,
            lane_count,
            max_bundle_size,
        };

        let (registration_tx, registration_rx) = oneshot::channel();
        let session =
            self.clone()
                .run_session(slot, name, init, max_chunk_size, requests, registration_tx);
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
    async fn dispatch(
        &self,
        request: Request<Streaming<DispatchRequest>>,
    ) -> Result<Response<DispatchResponse>, Status> {
        let mut requests = request.into_inner();

        let Some(dispatch_request::Request::Metadata(DispatchMetadata {
            session_token,
            peer_node_id,
            peer_address,
            bundle_size,
        })) = expect_first(
            &mut requests,
            self.tasks.cancel_token(),
            self.limits.handshake,
            "DispatchMetadata",
        )
        .await?
        .request
        else {
            return Err(Status::invalid_argument(
                "the first message must be DispatchMetadata",
            ));
        };

        let cla = self.resolve(session_token)?;
        let peer_node = peer_node_id
            .map(|node_id| node_id.parse::<NodeId>())
            .transpose()
            .map_err(|_| {
                Status::invalid_argument("DispatchMetadata.peer_node_id is not a valid node id")
            })?;
        let peer_address = peer_address.map(ClaAddress::try_from).transpose()?;

        let registered = cla
            .registered
            .get()
            .ok_or_else(|| Status::unavailable("registration closed"))?;
        if let Some((declared, limit)) = bundle_size.zip(registered.max_bundle_size)
            && declared > limit.get()
        {
            return Err(Status::resource_exhausted(format!(
                "a declared bundle of {declared} bytes exceeds the registration's limit of {limit} bytes"
            )));
        }

        let _transfer = transfer_slot(&cla.transfers, &cla.timeouts).await?;
        let mut receiver = BoundedChunkReceiver::new(
            &mut requests,
            &cla.timeouts,
            cla.max_chunk_size,
            bundle_size,
        )?;
        let acceptance = registered
            .sink
            .dispatch(peer_node.as_ref(), peer_address.as_ref(), &mut receiver)
            .await;
        // A chunk the receiver refused, or a stream the client cancelled, is
        // what the BPA's own refusal of the truncation that follows cannot
        // say, so it ends the call ahead of any verdict.
        if let Some(status) = receiver.into_error() {
            return Err(status);
        }
        match acceptance {
            Ok(acceptance) => Ok(Response::new(DispatchResponse {
                acceptance: Acceptance::from(acceptance).into(),
            })),
            // The receiver recorded no ending of its own, so the stream was
            // ended from the BPA's side: the registration is being torn down,
            // which cancels its streams before it tells this session.
            Err(ClaError::StreamCancelled) => Err(Status::unavailable("registration closed")),
            Err(error) => Err(cla_status(error)),
        }
    }

    type ForwardStream = ReceiverStream<Result<ForwardResponse, Status>>;

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn forward(
        &self,
        request: Request<Streaming<ForwardRequest>>,
    ) -> Result<Response<Self::ForwardStream>, Status> {
        let mut requests = request.into_inner();

        let Some(forward_request::Request::Metadata(ForwardMetadata {
            session_token,
            bundle_id,
        })) = expect_first(
            &mut requests,
            self.tasks.cancel_token(),
            self.limits.handshake,
            "ForwardMetadata",
        )
        .await?
        .request
        else {
            return Err(Status::invalid_argument(
                "the first message must be ForwardMetadata",
            ));
        };

        let cla = self.resolve(session_token)?;
        let bundle_id = BundleId::from_key(&bundle_id).map_err(|_| {
            Status::invalid_argument("ForwardMetadata.bundle_id is not a valid bundle id")
        })?;
        let Some(responses_rx) = cla.forwardings.collect(&bundle_id, requests) else {
            return Err(Status::not_found("no such Forwarding"));
        };
        Ok(Response::new(responses_rx))
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn add_peer(
        &self,
        request: Request<AddPeerRequest>,
    ) -> Result<Response<AddPeerResponse>, Status> {
        let AddPeerRequest {
            session_token,
            node_ids,
            address,
        } = request.into_inner();
        let cla = self.resolve(session_token)?;
        let address: ClaAddress = address
            .ok_or_else(|| Status::invalid_argument("AddPeerRequest.address is required"))?
            .try_into()?;
        let node_ids = node_ids
            .into_iter()
            .map(|node_id| node_id.parse::<NodeId>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                Status::invalid_argument("AddPeerRequest.node_ids holds an invalid node id")
            })?;

        let registered = cla
            .registered
            .get()
            .ok_or_else(|| Status::unavailable("registration closed"))?;
        let added = cla
            .alive(registered.sink.add_peer(address, &node_ids))
            .await?
            .map_err(cla_status)?;
        Ok(Response::new(AddPeerResponse { added }))
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn remove_peer(
        &self,
        request: Request<RemovePeerRequest>,
    ) -> Result<Response<RemovePeerResponse>, Status> {
        let RemovePeerRequest {
            session_token,
            address,
        } = request.into_inner();
        let cla = self.resolve(session_token)?;
        let address: ClaAddress = address
            .ok_or_else(|| Status::invalid_argument("RemovePeerRequest.address is required"))?
            .try_into()?;

        let registered = cla
            .registered
            .get()
            .ok_or_else(|| Status::unavailable("registration closed"))?;
        let removed = cla
            .alive(registered.sink.remove_peer(&address))
            .await?
            .map_err(cla_status)?;
        Ok(Response::new(RemovePeerResponse { removed }))
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn report_transfer_outcome(
        &self,
        request: Request<ReportTransferOutcomeRequest>,
    ) -> Result<Response<ReportTransferOutcomeResponse>, Status> {
        let ReportTransferOutcomeRequest {
            session_token,
            bundle_id,
            outcome,
        } = request.into_inner();
        let cla = self.resolve(session_token)?;
        let bundle_id = BundleId::from_key(&bundle_id).map_err(|_| {
            Status::invalid_argument(
                "ReportTransferOutcomeRequest.bundle_id is not a valid bundle id",
            )
        })?;
        let outcome = match outcome {
            Some(report_transfer_outcome_request::Outcome::Completed(_)) => {
                TransferOutcome::Completed
            }
            Some(report_transfer_outcome_request::Outcome::Failed(_)) => TransferOutcome::Failed,
            None => {
                return Err(Status::invalid_argument(
                    "ReportTransferOutcomeRequest.outcome is required",
                ));
            }
        };

        let registered = cla
            .registered
            .get()
            .ok_or_else(|| Status::unavailable("registration closed"))?;
        cla.alive(registered.sink.transfer_outcome(&bundle_id, outcome))
            .await?
            .map_err(cla_status)?;
        Ok(Response::new(ReportTransferOutcomeResponse {}))
    }
}
