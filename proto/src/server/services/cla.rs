// The `hardy.cla.v1` service. `Subscribe` opens a registration
// session, `Dispatch` streams inbound bundles into the BPA, and
// outbound bundles are announced on the session stream and collected
// by a `Forward` call.

use core::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::TaskPool;
use hardy_bpa::{
    Bytes, async_trait,
    bpa::BpaRegistration,
    cla::{self, Cla, Error, ForwardBundleResult, TransferOutcome},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{bundle::Id as BundleId, eid::NodeId};
use tokio::sync::oneshot;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::{Instrument, Span, instrument, trace_span};
use tracing::{debug, error, warn};

use crate::{
    MAX_LANE_COUNT,
    cla::{
        Acceptance, AddPeerRequest, AddPeerResponse, ClaAddressType, DispatchMetadata,
        DispatchRequest, DispatchResponse, ForwardMetadata, ForwardRequest, ForwardResponse,
        Forwarding, Registration, RemovePeerRequest, RemovePeerResponse,
        ReportTransferOutcomeRequest, ReportTransferOutcomeResponse, SubscribeRequest,
        SubscribeResponse, cla_service_server::ClaService, dispatch_request, forward_request,
        forward_result, report_transfer_outcome_request, subscribe_request, subscribe_response,
    },
    server::{
        Limits,
        adapter::{RequestReader, ResponseWriter},
        announce::{Announcements, Collection},
        error,
        session::{Session, SessionStream},
    },
    status::embed_cla_error,
    token::Token,
    transfer::Writer,
};

// The surface name used in spans, lease warnings, and the token `sub`.
const LABEL: &str = "cla";

// A subscription's event buffer, in messages.
const EVENT_DEPTH: usize = 16;

// Maps a BPA CLA error to a gRPC status, embedding the typed
// discriminator for the SDK.
fn cla_status(error: Error) -> Status {
    let status = match &error {
        Error::AlreadyExists(_) => Status::already_exists(error.to_string()),
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::StreamCancelled => Status::cancelled(error.to_string()),
        Error::PayloadTooLarge { .. } | Error::PayloadUnaddressable { .. } => {
            Status::resource_exhausted(error.to_string())
        }
        Error::PayloadUnderrun { .. } => Status::invalid_argument(error.to_string()),
        // May carry host detail: logged here, redacted to a generic
        // status on the wire.
        Error::Internal(e) => {
            error!("internal cla error: {e}");
            Status::internal("internal error")
        }
    };
    embed_cla_error(status, &error)
}

// Published by `on_register`: the BPA sink and the negotiated size cap.
#[derive(Clone)]
struct Registered {
    sink: Arc<dyn cla::Sink>,
    max_bundle_size: Option<NonZeroU64>,
}

// The per-session `Cla` registered with the BPA.
struct GrpcCla {
    session: Session<SubscribeResponse, Registered>,
    forwardings: Announcements<ForwardResponse, ForwardRequest>,
}

impl GrpcCla {
    fn new(session: Session<SubscribeResponse, Registered>) -> Self {
        Self {
            forwardings: Announcements::new(session.leases().clone()),
            session,
        }
    }

    async fn event(&self, event: subscribe_response::Event) -> cla::Result<()> {
        self.session
            .event(SubscribeResponse { event: Some(event) })
            .await
            .map_err(cla::Error::from)
    }

    // Waits for the Result or Cancel that settles the forwarding,
    // ignoring other messages. Cancel-safe: the `select!` callers drop
    // and re-create it.
    async fn answered(
        requests: &mut Streaming<ForwardRequest>,
    ) -> error::Result<ForwardBundleResult> {
        let mut warned = false;
        loop {
            match requests.message().await {
                Ok(Some(ForwardRequest {
                    request: Some(forward_request::Request::Result(result)),
                })) => match result.result {
                    Some(forward_result::Result::Sent(_)) => return Ok(ForwardBundleResult::Sent),
                    Some(forward_result::Result::NoNeighbour(_)) => {
                        return Ok(ForwardBundleResult::NoNeighbour);
                    }
                    Some(forward_result::Result::Accepted(_)) => {
                        return Ok(ForwardBundleResult::Accepted);
                    }
                    None => return Err(error::Error::ProtocolViolation),
                },
                Ok(Some(ForwardRequest {
                    request: Some(forward_request::Request::Cancel(_)),
                })) => return Err(error::Error::Cancelled),
                // The message may carry the session token; never
                // Debug-format it.
                Ok(Some(_)) if !warned => {
                    warned = true;
                    warn!("Ignoring unexpected message on the Forward request side");
                }
                Ok(Some(_)) => {}
                Ok(None) => return Err(error::Error::RequestStreamClosed),
                Err(e) => {
                    debug!("Forward stream failed: {e}");
                    return Err(error::Error::RequestStreamFailed);
                }
            }
        }
    }
}

#[async_trait]
impl Cla for GrpcCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        self.session.register(Registered {
            sink: Arc::from(sink),
            max_bundle_size,
        });
    }

    async fn on_unregister(&self) {
        self.session.abort();
    }

    async fn forward(
        &self,
        lane: Option<u32>,
        cla_addr: &cla::ClaAddress,
        bundle_id: &BundleId,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        let announced = self.forwardings.announce(bundle_id).map_err(|e| {
            warn!("Refusing a second announcement for a bundle already being forwarded");
            cla::Error::from(e)
        })?;

        self.event(subscribe_response::Event::Forwarding(Forwarding {
            bundle_id: bundle_id.to_key(),
            address: Some(cla_addr.clone().into()),
            lane,
            bundle_size: total_len,
        }))
        .await?;

        let Collection {
            responses_tx,
            mut requests,
        } = announced.collected().await?;
        let leases = self.session.leases();

        // A result may arrive mid-transfer; it always ends the transfer.
        let writer = ResponseWriter::new(&responses_tx, leases, stream);
        tokio::select! {
            biased;
            // Only `no_neighbour` is valid mid-transfer.
            answered = Self::answered(&mut requests) => {
                let e = match answered {
                    Ok(ForwardBundleResult::NoNeighbour) => {
                        return Ok(ForwardBundleResult::NoNeighbour);
                    }
                    Ok(ForwardBundleResult::Sent | ForwardBundleResult::Accepted) => {
                        error::Error::ProtocolViolation
                    }
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

        // Chunks sent, wait for the result. Deliberately unleased:
        // only session teardown bounds the wait.
        tokio::select! {
            biased;
            _ = leases.cancelled() => {
                if let Some(status) = error::Error::SessionClosed.status() {
                    let _ = responses_tx.try_send(Err(status));
                }
                Err(error::Error::SessionClosed.into())
            }
            answered = Self::answered(&mut requests) => match answered {
                Ok(result) => Ok(result),
                Err(e) => {
                    if let Some(status) = e.status() {
                        let _ = responses_tx.try_send(Err(status));
                    }
                    Err(e.into())
                }
            },
        }
    }
}

/// The server implementation of the `hardy.cla.v1` service.
///
/// Clones share one set of sessions. Shut down the pool given to
/// [`new`](Self::new) only after the transport has stopped accepting:
/// pool shutdown tears down every subscription.
#[derive(Clone)]
pub struct ClaServiceImpl {
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    limits: Limits,
    sessions: Arc<DashMap<Token, Arc<GrpcCla>, RandomState>>,
    #[cfg(test)]
    hooks: super::tests::Hooks,
}

impl ClaServiceImpl {
    /// Creates the service with default [`Limits`].
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self::with_limits(bpa, tasks, Limits::default())
    }

    /// Creates the service with the given [`Limits`].
    ///
    /// This surface does not use the `ack` deadline.
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

        // An unspecified type means none; an unrecognised value is refused.
        let address_type = match register.address_type {
            Some(t) => match ClaAddressType::try_from(t) {
                Ok(address_type) => Option::<cla::ClaAddressType>::from(address_type),
                Err(_) => {
                    let _ = response_tx.send(Err(Status::invalid_argument(format!(
                        "Unknown address_type {t}"
                    ))));
                    return;
                }
            },
            None => None,
        };

        // Bounded: the BPA sizes per-peer egress queues from this count.
        let lane_count = match register.lane_count {
            Some(0) => {
                let _ =
                    response_tx.send(Err(Status::invalid_argument("lane_count must not be zero")));
                return;
            }
            Some(n) if n > MAX_LANE_COUNT => {
                let _ = response_tx.send(Err(Status::invalid_argument(format!(
                    "lane_count {n} exceeds the maximum of {MAX_LANE_COUNT}"
                ))));
                return;
            }
            Some(n) => NonZeroU32::new(n),
            None => None,
        };
        let max_bundle_size = match register.max_bundle_size {
            Some(0) => {
                let _ = response_tx.send(Err(Status::invalid_argument(
                    "max_bundle_size must not be zero",
                )));
                return;
            }
            Some(n) => NonZeroU64::new(n),
            None => None,
        };

        // Created before the BPA registration: events raised during
        // registration buffer in the session.
        let token = Token::mint(&format!("{LABEL}:{}", register.name));
        let session = Session::new(self.tasks.child_token(), LABEL, self.limits, EVENT_DEPTH);
        let cla = Arc::new(GrpcCla::new(session));

        let node_ids = match self
            .bpa
            .register_cla(
                register.name,
                cla.clone(),
                None,
                cla::ClaInit {
                    address_type,
                    lane_count,
                    max_bundle_size,
                },
            )
            .await
        {
            Ok(node_ids) => node_ids,
            Err(e) => {
                let _ = response_tx.send(Err(cla_status(e)));
                return;
            }
        };

        let Some(registered) = cla.session.registered() else {
            warn!("register_cla returned without driving on_register");
            let _ = response_tx.send(Err(Status::internal("internal error")));
            cla.session.abort();
            return;
        };

        self.sessions.insert(token.clone(), cla.clone());

        let registration = SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(Registration {
                node_ids: node_ids.iter().map(ToString::to_string).collect(),
                session_token: token.clone().into(),
                max_bundle_size: registered.max_bundle_size.map(NonZeroU64::get),
            })),
        };
        let mut stream = cla.session.open(registration);
        let _guard = stream.cancel_guard();

        if response_tx.send(Ok(Response::new(stream))).is_ok() {
            cla.session.serve(requests).await;
        }

        // In order: stop the work, drop the token, unregister; the
        // stream ends last.
        cla.session.abort();
        self.sessions.remove(&token);
        if let Some(registered) = cla.session.registered() {
            registered.sink.unregister().await;
        }
        #[cfg(test)]
        let _ = self.hooks.torn_down.send(token);
    }

    fn resolve(&self, token: Bytes) -> Result<Arc<GrpcCla>, Status> {
        self.sessions
            .get(&Token::from(token))
            .map(|cla| cla.clone())
            .ok_or_else(|| Status::unauthenticated("Unknown session token"))
    }
}

#[async_trait]
impl ClaService for ClaServiceImpl {
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
    async fn dispatch(
        &self,
        request: Request<Streaming<DispatchRequest>>,
    ) -> Result<Response<DispatchResponse>, Status> {
        let mut requests = request.into_inner();

        let Some(dispatch_request::Request::Metadata(DispatchMetadata {
            session_token,
            peer_node_id,
            peer_addr,
        })) = requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be the metadata",
            ));
        };
        let cla = self.resolve(session_token)?;
        let peer_node = peer_node_id
            .map(|s| s.parse::<NodeId>())
            .transpose()
            .map_err(|e| Status::invalid_argument(format!("Invalid peer_node_id: {e}")))?;
        let peer_addr = peer_addr.map(cla::ClaAddress::try_from).transpose()?;

        let mut reader = RequestReader::new(requests, cla.session.leases().clone(), "Dispatch");
        match cla
            .session
            .registered()
            .ok_or_else(|| cla_status(Error::Disconnected))?
            .sink
            .dispatch(peer_node.as_ref(), peer_addr.as_ref(), &mut reader)
            .await
        {
            // The BPA reports acceptance as `Ok` (see `docs/TODO.md`).
            Ok(()) => Ok(Response::new(DispatchResponse {
                acceptance: Acceptance::Accepted.into(),
            })),
            Err(e) => Err(reader.status().unwrap_or_else(|| cla_status(e))),
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
        })) = requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be the metadata",
            ));
        };
        let cla = self.resolve(session_token)?;
        let Some(responses_rx) = cla.forwardings.collect(&bundle_id, requests) else {
            return Err(Status::not_found("No such forwarding"));
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
        let address: cla::ClaAddress = address
            .ok_or_else(|| Status::invalid_argument("Missing address"))?
            .try_into()?;
        let node_ids = node_ids
            .into_iter()
            .map(|s| s.parse::<NodeId>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Status::invalid_argument(format!("Invalid node id: {e}")))?;

        let added = cla
            .session
            .registered()
            .ok_or_else(|| cla_status(Error::Disconnected))?
            .sink
            .add_peer(address, &node_ids)
            .await
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
        let address: cla::ClaAddress = address
            .ok_or_else(|| Status::invalid_argument("Missing address"))?
            .try_into()?;

        let removed = cla
            .session
            .registered()
            .ok_or_else(|| cla_status(Error::Disconnected))?
            .sink
            .remove_peer(&address)
            .await
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
        let bundle_id = BundleId::from_key(&bundle_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;
        let outcome = match outcome {
            Some(report_transfer_outcome_request::Outcome::Completed(_)) => {
                TransferOutcome::Completed
            }
            Some(report_transfer_outcome_request::Outcome::Failed(_)) => TransferOutcome::Failed,
            None => return Err(Status::invalid_argument("Missing outcome")),
        };

        cla.session
            .registered()
            .ok_or_else(|| cla_status(Error::Disconnected))?
            .sink
            .transfer_outcome(&bundle_id, outcome)
            .await
            .map_err(cla_status)?;
        Ok(Response::new(ReportTransferOutcomeResponse {}))
    }
}

// Wire tests against a real BPA.
#[cfg(test)]
mod tests {
    #[cfg(feature = "client")]
    use core::num::{NonZeroU32, NonZeroU64};

    use std::{net::SocketAddr, time::Duration};

    #[cfg(feature = "client")]
    use crate::client::BpaClient;
    #[cfg(feature = "client")]
    use hardy_async::sync::spin::Once;
    #[cfg(feature = "client")]
    use hardy_bpa::stream::concat_stream;
    use hardy_bpa::{Bytes, bpa::Bpa};
    use tokio::sync::mpsc;
    use tonic::{
        Code,
        transport::{Channel, Server},
    };

    use super::{
        super::tests::{build_bpa, build_bundle, ipn1, serve, timeout, wait_torn_down},
        *,
    };
    use crate::{
        cla::{
            ClaAddress, ForwardResult, Register, Unregister, cla_service_client::ClaServiceClient,
            cla_service_server::ClaServiceServer, forward_response,
        },
        server::announce::DATA_CHANNEL_DEPTH,
    };

    struct Harness {
        bpa: Arc<Bpa>,
        // Held live: dropping the pool would tear down the sessions.
        #[expect(dead_code, reason = "held for its liveness")]
        tasks: TaskPool,
        client: ClaServiceClient<Channel>,
        #[cfg_attr(
            not(feature = "client"),
            expect(dead_code, reason = "read by the client SDK test")
        )]
        address: SocketAddr,
        // A second handle on the surface, for the teardown barrier.
        surface: ClaServiceImpl,
    }

    // A running BPA (node ipn:1) behind the surface on a port-0
    // listener, plus a connected generated client.
    async fn harness() -> Harness {
        harness_with_limits(Limits::default()).await
    }

    async fn harness_with_limits(limits: Limits) -> Harness {
        let bpa = build_bpa(ipn1(), false).await;

        let tasks = TaskPool::new();
        let surface = ClaServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits);
        let service = ClaServiceServer::new(surface.clone());
        let address = serve(Server::builder().add_service(service)).await;

        let client = ClaServiceClient::connect(format!("http://{address}"))
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
        node_ids: Vec<String>,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(client: &mut ClaServiceClient<Channel>, name: &str) -> Registered {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: name.to_string(),
                    address_type: Some(ClaAddressType::Tcp.into()),
                    lane_count: None,
                    max_bundle_size: None,
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
            node_ids: registration.node_ids,
            token: registration.session_token,
        }
    }

    fn tcp_address(address: &str) -> ClaAddress {
        ClaAddress {
            address_type: ClaAddressType::Tcp.into(),
            address: Bytes::copy_from_slice(address.as_bytes()),
        }
    }

    async fn add_peer(
        client: &mut ClaServiceClient<Channel>,
        token: Bytes,
        node_ids: &[&str],
        address: &str,
    ) -> Result<AddPeerResponse, Status> {
        client
            .add_peer(AddPeerRequest {
                session_token: token,
                node_ids: node_ids.iter().map(|s| s.to_string()).collect(),
                address: Some(tcp_address(address)),
            })
            .await
            .map(|response| response.into_inner())
    }

    async fn dispatch(
        client: &mut ClaServiceClient<Channel>,
        token: Bytes,
        bundle: Bytes,
    ) -> Result<DispatchResponse, Status> {
        let messages = [
            DispatchRequest {
                request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                    session_token: token,
                    peer_node_id: None,
                    peer_addr: None,
                })),
            },
            DispatchRequest {
                request: Some(dispatch_request::Request::LastChunk(bundle)),
            },
        ];
        client
            .dispatch(tokio_stream::iter(messages))
            .await
            .map(|response| response.into_inner())
    }

    async fn dispatch_chunked(client: &mut ClaServiceClient<Channel>, token: Bytes, bundle: &[u8]) {
        let mut messages = vec![DispatchRequest {
            request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                session_token: token,
                peer_node_id: None,
                peer_addr: None,
            })),
        }];
        for chunk in bundle.chunks(crate::CHUNK_SIZE) {
            messages.push(DispatchRequest {
                request: Some(dispatch_request::Request::Chunk(Bytes::copy_from_slice(
                    chunk,
                ))),
            });
        }
        messages.push(DispatchRequest {
            request: Some(dispatch_request::Request::LastChunk(Bytes::new())),
        });
        client.dispatch(tokio_stream::iter(messages)).await.unwrap();
    }

    // The announced bundle is not byte-identical to the dispatched
    // one: the BPA rewrites it at egress.
    async fn forwarding(registered: &mut Registered) -> Forwarding {
        let event = timeout(registered.events.message()).await.unwrap().unwrap();
        let Some(subscribe_response::Event::Forwarding(forwarding)) = event.event else {
            panic!("expected a Forwarding, got {event:?}");
        };
        forwarding
    }

    // Collects the streamed bundle, then answers `result`, or cancels
    // in band after the first chunk when `abandon` is set.
    async fn execute_forward(
        client: &mut ClaServiceClient<Channel>,
        token: Bytes,
        bundle_id: &str,
        result: forward_result::Result,
        abandon: bool,
    ) -> Result<Vec<u8>, Status> {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: token,
                    bundle_id: bundle_id.to_string(),
                })),
            })
            .await
            .unwrap();

        let mut stream = client
            .forward(ReceiverStream::new(requests_rx))
            .await?
            .into_inner();
        let mut collected = Vec::new();
        let mut cancelled = false;
        loop {
            match stream.message().await?.and_then(|r| r.response) {
                Some(forward_response::Response::Chunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    if abandon && !cancelled {
                        cancelled = true;
                        let _ = requests_tx
                            .send(ForwardRequest {
                                request: Some(forward_request::Request::Cancel(())),
                            })
                            .await;
                    }
                }
                Some(forward_response::Response::LastChunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    requests_tx
                        .send(ForwardRequest {
                            request: Some(forward_request::Request::Result(ForwardResult {
                                result: Some(result),
                            })),
                        })
                        .await
                        .unwrap();
                    // The result completes the call: the stream ends
                    // cleanly.
                    assert!(timeout(stream.message()).await?.is_none());
                    return Ok(collected);
                }
                // The server closed after the Cancel; return what
                // arrived.
                None if abandon => return Ok(collected),
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
    }

    // Unregisters `registered`, registers afresh, and completes the
    // re-offered forwarding, asserting the whole bundle arrives.
    async fn reforwarded_after_reregistration(
        harness: &mut Harness,
        registered: Registered,
        bundle_size: u64,
    ) {
        let Registered {
            requests_tx,
            mut events,
            token,
            ..
        } = registered;
        // Subscribe before triggering teardown so the re-registration
        // below cannot race the name going free.
        let mut torn = harness.surface.hooks.torn_down.subscribe();
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(timeout(events.message()).await.unwrap().is_none());
        wait_torn_down(&mut torn, &token).await;

        let mut registered = register(&mut harness.client, "test-cla").await;
        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();

        let requeued = forwarding(&mut registered).await;
        assert_eq!(requeued.bundle_size, bundle_size);
        let executed = execute_forward(
            &mut harness.client,
            registered.token.clone(),
            &requeued.bundle_id,
            forward_result::Result::Sent(()),
            false,
        )
        .await
        .unwrap();
        assert_eq!(executed.len() as u64, bundle_size);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registration_returns_node_ids_and_a_token() {
        let mut harness = harness().await;

        let registered = register(&mut harness.client, "test-cla").await;
        assert_eq!(registered.node_ids.len(), 1);
        assert!(registered.node_ids[0].starts_with("ipn:1"));

        // A second registration with the same name is rejected.
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "test-cla".to_string(),
                    address_type: None,
                    lane_count: None,
                    max_bundle_size: None,
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::AlreadyExists);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_and_forward_roundtrip() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;

        let peer = "127.0.0.1:4556";
        let added = add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            peer,
        )
        .await
        .unwrap();
        assert!(added.added);

        // A bundle from the link for a destination behind the peer:
        // the BPA routes it back out through this CLA.
        let payload = b"across the wire and back out";
        let bundle = build_bundle("ipn:3.1", "ipn:2.1", payload);
        let dispatched = dispatch(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap();
        assert_eq!(
            dispatched.acceptance(),
            Acceptance::Accepted,
            "a bundle the BPA took must be answered accepted, so the CLA acknowledges it"
        );

        let forwarding = forwarding(&mut registered).await;
        assert_eq!(forwarding.address, Some(tcp_address(peer)));

        // The streamed bundle is exactly the announced size, carries
        // the payload, and the result completes the forwarding.
        let executed = execute_forward(
            &mut harness.client,
            registered.token.clone(),
            &forwarding.bundle_id,
            forward_result::Result::Sent(()),
            false,
        )
        .await
        .unwrap();
        assert_eq!(executed.len() as u64, forwarding.bundle_size);
        assert!(executed.windows(payload.len()).any(|w| w == payload));

        harness.bpa.shutdown().await;
    }

    #[ignore = "the BPA parks an interrupted forward instead of redispatching it: see docs/TODO.md"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_forwarding_stays_queued() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;

        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();

        // A bundle bigger than one wire chunk, so the cancel lands
        // before the last chunk.
        let payload = vec![0x5a; crate::CHUNK_SIZE + 3];
        let bundle = build_bundle("ipn:3.1", "ipn:2.1", &payload);
        dispatch(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap();
        let announced = forwarding(&mut registered).await;

        // The cancel ends the call cleanly with no result recorded.
        execute_forward(
            &mut harness.client,
            registered.token.clone(),
            &announced.bundle_id,
            forward_result::Result::Sent(()),
            true,
        )
        .await
        .expect("a cancelled forwarding ends cleanly");

        // The abandoned forward fails, so the BPA requeues the bundle
        // and announces it again.
        let requeued = forwarding(&mut registered).await;
        let executed = execute_forward(
            &mut harness.client,
            registered.token.clone(),
            &requeued.bundle_id,
            forward_result::Result::Sent(()),
            false,
        )
        .await
        .unwrap();
        assert_eq!(executed.len() as u64, requeued.bundle_size);
        assert!(executed.windows(payload.len()).any(|w| w == payload));

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_accepted_forwarding_reports_its_outcome() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;

        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();
        let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"deferred outcome");
        dispatch(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap();
        let forwarding = forwarding(&mut registered).await;

        // The CLA takes ownership; the BPA holds the bundle awaiting
        // the outcome.
        execute_forward(
            &mut harness.client,
            registered.token.clone(),
            &forwarding.bundle_id,
            forward_result::Result::Accepted(()),
            false,
        )
        .await
        .unwrap();

        harness
            .client
            .report_transfer_outcome(ReportTransferOutcomeRequest {
                session_token: registered.token.clone(),
                bundle_id: forwarding.bundle_id.clone(),
                outcome: Some(report_transfer_outcome_request::Outcome::Completed(())),
            })
            .await
            .unwrap();

        // A second outcome for a resolved transfer is accepted and
        // ignored.
        harness
            .client
            .report_transfer_outcome(ReportTransferOutcomeRequest {
                session_token: registered.token.clone(),
                bundle_id: forwarding.bundle_id,
                outcome: Some(report_transfer_outcome_request::Outcome::Failed(())),
            })
            .await
            .unwrap();

        harness.bpa.shutdown().await;
    }

    // A `sent` before the last chunk claims bytes the CLA never
    // received. The server refuses it and the bundle stays with the
    // BPA, which offers it to the next registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_early_sent_never_completes_the_forwarding() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;

        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();

        // More than the response buffer plus the transport windows can
        // absorb, so the transfer cannot complete before the early
        // result is seen. Derived from the constant so a deeper buffer
        // cannot make the test vacuous.
        let payload = vec![0x5a; DATA_CHANNEL_DEPTH * 4 * crate::CHUNK_SIZE];
        let bundle = build_bundle("ipn:3.1", "ipn:2.1", &payload);
        dispatch_chunked(&mut harness.client, registered.token.clone(), &bundle).await;
        let announced = forwarding(&mut registered).await;

        // `sent` straight after the metadata, before reading a single
        // chunk.
        let (requests_tx, requests_rx) = mpsc::channel(2);
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: registered.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Result(ForwardResult {
                    result: Some(forward_result::Result::Sent(())),
                })),
            })
            .await
            .unwrap();
        let mut stream = harness
            .client
            .forward(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();

        // The stream must end without its last chunk. The
        // INVALID_ARGUMENT refusal is best-effort: it goes out via
        // `try_send` against a possibly full response channel, so the
        // call may end as a bare truncation instead.
        loop {
            match timeout(stream.message()).await {
                Ok(Some(ForwardResponse {
                    response: Some(forward_response::Response::Chunk(_)),
                })) => continue,
                Ok(Some(ForwardResponse {
                    response: Some(forward_response::Response::LastChunk(_)),
                })) => panic!("an early result must never be given the whole bundle"),
                Ok(Some(_)) | Ok(None) | Err(_) => break,
            }
        }

        // The bundle stays with the BPA and is re-offered.
        reforwarded_after_reregistration(&mut harness, registered, announced.bundle_size).await;

        harness.bpa.shutdown().await;
    }

    // An empty `ForwardResult` answers nothing, and the wait it
    // arrives in is unleased, so ignoring it could park the egress
    // queue forever. The server ends the call instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_result_ends_the_forwarding() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;

        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();

        let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"an unanswerable forwarding");
        dispatch(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap();
        let announced = forwarding(&mut registered).await;

        let (requests_tx, requests_rx) = mpsc::channel(2);
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: registered.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        let mut stream = harness
            .client
            .forward(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();

        // Drain to the last chunk so the empty result lands in the
        // unleased wait and the refusal has room on the response side.
        loop {
            match timeout(stream.message()).await.unwrap().unwrap().response {
                Some(forward_response::Response::Chunk(_)) => {}
                Some(forward_response::Response::LastChunk(_)) => break,
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Result(ForwardResult {
                    result: None,
                })),
            })
            .await
            .unwrap();

        let status = timeout(stream.message())
            .await
            .expect_err("an empty result must end the call");
        assert_eq!(status.code(), Code::InvalidArgument);

        // Unanswered, so the bundle is still the BPA's to re-offer.
        reforwarded_after_reregistration(&mut harness, registered, announced.bundle_size).await;

        harness.bpa.shutdown().await;
    }

    // An unrecognised `address_type` is refused rather than silently
    // read as unspecified.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_address_type_is_refused() {
        let mut harness = harness().await;

        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "test-cla".to_string(),
                    address_type: Some(i32::MAX),
                    lane_count: None,
                    max_bundle_size: None,
                })),
            })
            .await
            .unwrap();

        let status = harness
            .client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);

        // The refusal registered nothing, so the name is still free.
        register(&mut harness.client, "test-cla").await;

        harness.bpa.shutdown().await;
    }

    // The claim lease is the only bound on a forwarding the CLA never
    // collects. A zero claim lease expires at once and tears the
    // session down; teardown is the barrier the test waits on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_uncollected_forwarding_expires_its_claim_and_ends_the_session() {
        let mut harness = harness_with_limits(Limits {
            claim: Duration::ZERO,
            ..Limits::default()
        })
        .await;
        let mut torn = harness.surface.hooks.torn_down.subscribe();
        let mut registered = register(&mut harness.client, "test-cla").await;

        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();
        dispatch(
            &mut harness.client,
            registered.token.clone(),
            build_bundle("ipn:3.1", "ipn:2.1", b"never collected"),
        )
        .await
        .unwrap();

        // The announcement reaches the CLA; nothing collects it.
        forwarding(&mut registered).await;

        // The client holds its stream open and never unregisters, so
        // only the expired claim can reach this barrier.
        wait_torn_down(&mut torn, &registered.token).await;

        harness.bpa.shutdown().await;
    }

    #[ignore = "the BPA commits a truncated dispatch until the final-segment gate lands: see docs/TODO.md"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_truncated_dispatch_never_commits() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;

        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();

        // A Chunk but no LastChunk: the half-close is a truncation.
        let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"cut short");
        let messages = [
            DispatchRequest {
                request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                    session_token: registered.token.clone(),
                    peer_node_id: None,
                    peer_addr: None,
                })),
            },
            DispatchRequest {
                request: Some(dispatch_request::Request::Chunk(bundle)),
            },
        ];
        let status = harness
            .client
            .dispatch(tokio_stream::iter(messages))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Aborted);

        // `shutdown()` joins the BPA workers, so anything to announce
        // has been announced and the session stream then ends; drain
        // it and assert no Forwarding arrived.
        harness.bpa.shutdown().await;
        while let Some(event) = registered.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Forwarding(_))),
                "a truncated dispatch must not forward"
            );
        }
    }

    #[ignore = "the BPA commits a cancelled dispatch until the final-segment gate lands: see docs/TODO.md"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_dispatch_is_discarded() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;

        // A live peer for the destination, so a committed bundle would
        // surface as a Forwarding in the drain below.
        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();

        let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"undone");
        let messages = [
            DispatchRequest {
                request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                    session_token: registered.token.clone(),
                    peer_node_id: None,
                    peer_addr: None,
                })),
            },
            DispatchRequest {
                request: Some(dispatch_request::Request::Chunk(bundle)),
            },
            DispatchRequest {
                request: Some(dispatch_request::Request::Cancel(())),
            },
        ];
        let status = harness
            .client
            .dispatch(tokio_stream::iter(messages))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Cancelled);

        // `shutdown()` joins the BPA workers, so anything to announce
        // has been announced and the session stream then ends; drain
        // it and assert no Forwarding arrived.
        harness.bpa.shutdown().await;
        while let Some(event) = registered.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Forwarding(_))),
                "a cancelled dispatch must not commit"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn peers_are_added_and_removed_once() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, "test-cla").await;

        let peer = "127.0.0.1:4556";
        assert!(
            add_peer(
                &mut harness.client,
                registered.token.clone(),
                &["ipn:2.0"],
                peer
            )
            .await
            .unwrap()
            .added
        );
        assert!(
            !add_peer(
                &mut harness.client,
                registered.token.clone(),
                &["ipn:2.0"],
                peer
            )
            .await
            .unwrap()
            .added
        );

        let remove = |client: &mut ClaServiceClient<Channel>, token: Bytes| {
            let request = RemovePeerRequest {
                session_token: token,
                address: Some(tcp_address(peer)),
            };
            let mut client = client.clone();
            async move { client.remove_peer(request).await }
        };
        assert!(
            remove(&mut harness.client, registered.token.clone())
                .await
                .unwrap()
                .into_inner()
                .removed
        );
        assert!(
            !remove(&mut harness.client, registered.token.clone())
                .await
                .unwrap()
                .into_inner()
                .removed
        );

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forged_token_is_rejected() {
        let mut harness = harness().await;
        register(&mut harness.client, "test-cla").await;

        let status = add_peer(
            &mut harness.client,
            Bytes::from_static(b"forged"),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_stream_tears_the_session_down() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, "test-cla").await;

        // The client vanishes without Unregister. The teardown signal
        // fires after the token is removed and the CLA unregistered, so
        // the rejection and re-registration below are race-free.
        let mut torn = harness.surface.hooks.torn_down.subscribe();
        drop(registered.events);
        drop(registered.requests_tx);
        wait_torn_down(&mut torn, &registered.token).await;

        // The token is dead.
        let status = add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        // Teardown freed the name, so a new registration succeeds on
        // the first try.
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "test-cla".to_string(),
                    address_type: None,
                    lane_count: None,
                    max_bundle_size: None,
                })),
            })
            .await
            .unwrap();
        harness
            .client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .unwrap();

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unregister_ends_the_session_and_invalidates_the_token() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;
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
        wait_torn_down(&mut torn, &registered.token).await;
        let status = add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forward_for_an_unknown_bundle_is_not_found() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, "test-cla").await;

        // Nothing was announced, so the Forward call matches no
        // forwarding.
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: registered.token.clone(),
                    bundle_id: "no-such-bundle".to_string(),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .forward(ReceiverStream::new(requests_rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::NotFound);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forward_requires_the_metadata_first() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, "test-cla").await;

        // A result before the metadata is a protocol error.
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Result(ForwardResult {
                    result: Some(forward_result::Result::Sent(())),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .forward(ReceiverStream::new(requests_rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);

        drop(registered);
        harness.bpa.shutdown().await;
    }

    // With a live call holding the announced forwarding, a duplicate
    // Forward for the same id answers NOT_FOUND and the live call
    // completes untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_duplicate_forward_for_a_live_call_is_not_found() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;
        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            "127.0.0.1:4556",
        )
        .await
        .unwrap();

        // Two wire chunks, so the first call can hold the forwarding
        // open after reading one.
        let payload = vec![0x5a; crate::CHUNK_SIZE + 3];
        let bundle = build_bundle("ipn:3.1", "ipn:2.1", &payload);
        dispatch(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap();
        let announced = forwarding(&mut registered).await;

        // Open the first Forward and read exactly one chunk.
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: registered.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        let mut live = harness
            .client
            .forward(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();
        let first = timeout(live.message()).await.unwrap().unwrap();
        let Some(forward_response::Response::Chunk(first)) = first.response else {
            panic!("expected the first chunk, got {first:?}");
        };
        let mut collected = first.to_vec();

        // The duplicate answers NOT_FOUND without touching the claim.
        let (dup_requests, dup_rx) = mpsc::channel(4);
        dup_requests
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: registered.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .forward(ReceiverStream::new(dup_rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::NotFound);

        // The live call is unharmed: it collects to the last chunk and
        // completes on its result.
        loop {
            match timeout(live.message())
                .await
                .unwrap()
                .unwrap()
                .response
                .unwrap()
            {
                forward_response::Response::Chunk(chunk) => collected.extend_from_slice(&chunk),
                forward_response::Response::LastChunk(chunk) => {
                    collected.extend_from_slice(&chunk);
                    break;
                }
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
        assert_eq!(collected.len() as u64, announced.bundle_size);
        requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Result(ForwardResult {
                    result: Some(forward_result::Result::Sent(())),
                })),
            })
            .await
            .unwrap();
        assert!(timeout(live.message()).await.unwrap().is_none());

        harness.bpa.shutdown().await;
    }

    // Zero and above-the-bound lane counts are INVALID_ARGUMENT; the
    // bound itself registers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lane_count_is_validated_at_registration() {
        let mut harness = harness().await;

        for lane_count in [Some(0), Some(MAX_LANE_COUNT + 1)] {
            let (requests_tx, requests_rx) = mpsc::channel(4);
            requests_tx
                .send(SubscribeRequest {
                    request: Some(subscribe_request::Request::Register(Register {
                        name: "bad-lanes".to_string(),
                        address_type: None,
                        lane_count,
                        max_bundle_size: None,
                    })),
                })
                .await
                .unwrap();
            let status = harness
                .client
                .subscribe(ReceiverStream::new(requests_rx))
                .await
                .unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument);
        }

        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "max-lanes".to_string(),
                    address_type: None,
                    lane_count: Some(MAX_LANE_COUNT),
                    max_bundle_size: None,
                })),
            })
            .await
            .unwrap();
        let mut events = harness
            .client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();
        let event = timeout(events.message()).await.unwrap().unwrap();
        assert!(matches!(
            event.event,
            Some(subscribe_response::Event::Registration(_))
        ));

        harness.bpa.shutdown().await;
    }

    // A CLA that consumes the stream, answers Accepted, and reports
    // the forwarded bundle id.
    #[cfg(feature = "client")]
    struct AcceptingCla {
        sink: Once<Box<dyn cla::Sink>>,
        forwarded: mpsc::Sender<BundleId>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl Cla for AcceptingCla {
        async fn on_register(
            &self,
            sink: Box<dyn cla::Sink>,
            _node_ids: &[NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            bundle_id: &BundleId,
            _total_len: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<ForwardBundleResult> {
            concat_stream(stream, usize::MAX, None)
                .await
                .map_err(|e| cla::Error::Internal(e.into()))?;
            let _ = self.forwarded.send(bundle_id.clone()).await;
            Ok(ForwardBundleResult::Accepted)
        }
    }

    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_sdk_deferred_outcome_completes_the_transfer() {
        let harness = harness().await;
        let client = BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let (forwarded_tx, mut forwarded_rx) = mpsc::channel(4);
        let cla = Arc::new(AcceptingCla {
            sink: Once::new(),
            forwarded: forwarded_tx,
        });
        let _handle = client
            .register_cla(
                "sdk-accepting".to_string(),
                cla.clone(),
                cla::ClaInit::default(),
            )
            .await
            .unwrap();

        let sink = cla.sink.get().unwrap();
        sink.add_peer(
            cla::ClaAddress::Tcp("127.0.0.1:4556".parse().unwrap()),
            &["ipn:2.0".parse().unwrap()],
        )
        .await
        .unwrap();

        let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"deferred");
        sink.dispatch(None, None, &mut bundle.clone())
            .await
            .unwrap();
        let accepted = timeout(forwarded_rx.recv()).await.unwrap();

        sink.transfer_outcome(&accepted, cla::TransferOutcome::Completed)
            .await
            .unwrap();

        // The shutdown joins the BPA's worker pool and ends the SDK
        // session, so draining the forward channel to its end must find
        // no re-offer.
        harness.bpa.shutdown().await;
        drop(cla);
        timeout(async {
            assert!(
                forwarded_rx.recv().await.is_none(),
                "a completed transfer must not be re-offered"
            );
        })
        .await;
    }

    // A no-op CLA for probing registration validation.
    #[cfg(feature = "client")]
    struct OverLanedCla;

    #[cfg(feature = "client")]
    #[async_trait]
    impl Cla for OverLanedCla {
        async fn on_register(
            &self,
            _sink: Box<dyn cla::Sink>,
            _node_ids: &[NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
        }

        async fn on_unregister(&self) {}

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            _bundle_id: &BundleId,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<ForwardBundleResult> {
            Ok(ForwardBundleResult::Sent)
        }
    }

    // The SDK refuses an over-declared lane count rather than clamp
    // it: a clamped registration would leave the CLA believing in
    // lanes the BPA never offers.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sdk_rejects_an_over_declared_lane_count() {
        let harness = harness().await;
        let client = BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let error = client
            .register_cla(
                "over-laned".to_string(),
                Arc::new(OverLanedCla),
                cla::ClaInit {
                    lane_count: NonZeroU32::new(MAX_LANE_COUNT + 1),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, cla::Error::Internal(_)),
            "expected the SDK to reject the declaration, got {error:?}"
        );

        harness.bpa.shutdown().await;
    }

    // A CLA that collects each forwarded bundle and answers Sent.
    #[cfg(feature = "client")]
    struct SdkCla {
        sink: Once<Box<dyn cla::Sink>>,
        forwarded: mpsc::Sender<Bytes>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl Cla for SdkCla {
        async fn on_register(
            &self,
            sink: Box<dyn cla::Sink>,
            _node_ids: &[NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            _bundle_id: &BundleId,
            _total_len: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<ForwardBundleResult> {
            let bundle = concat_stream(stream, usize::MAX, None)
                .await
                .map_err(|e| cla::Error::Internal(e.into()))?;
            let _ = self.forwarded.send(bundle).await;
            Ok(ForwardBundleResult::Sent)
        }
    }

    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_sdk_roundtrip() {
        let harness = harness().await;
        let client =
            BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

        let (forwarded_tx, mut forwarded_rx) = mpsc::channel(4);
        let cla = Arc::new(SdkCla {
            sink: Once::new(),
            forwarded: forwarded_tx,
        });
        let handle = client
            .register_cla(
                "sdk-cla".to_string(),
                cla.clone(),
                cla::ClaInit {
                    address_type: Some(cla::ClaAddressType::Tcp),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!handle.id().is_empty());

        // Announce a peer, then dispatch a bundle destined for it: the
        // BPA routes it back out through the CLA.
        let sink = cla.sink.get().unwrap();
        let peer = cla::ClaAddress::try_from((
            cla::ClaAddressType::Tcp,
            Bytes::from_static(b"127.0.0.1:4556"),
        ))
        .unwrap();
        sink.add_peer(peer, &["ipn:2.0".parse().unwrap()])
            .await
            .unwrap();

        let payload = b"through the sdk to the link";
        let bundle = build_bundle("ipn:3.1", "ipn:2.1", payload);
        sink.dispatch(None, None, &mut bundle.clone())
            .await
            .unwrap();

        // The BPA rewrites at egress, so the forwarded bytes differ but
        // the payload survives.
        let forwarded = timeout(forwarded_rx.recv()).await.unwrap();
        assert!(forwarded.windows(payload.len()).any(|w| w == payload));

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }
}
