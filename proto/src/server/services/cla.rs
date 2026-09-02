// The CLA surface: the `cla.v1` wire served against the
// convergence-layer surface of a BPA. Declarations are ordered
// define-before-reference: the wire conversions, the component as the
// BPA sees it, the doors' streaming halves, then the rpc service;
// within the service, the handlers mirror the schema's order. Two
// shapes are new against the template: Dispatch streams straight into
// the BPA through `Sink::dispatch`, and Forward is a
// rendezvous: `Cla::forward_streamed` announces a `Forwarding` and
// parks a one-shot, the bidi door hands its streams back through it,
// and `forward_streamed` drives the transfer inline so the BPA's
// stream flows straight down the wire without materialising in the
// bridge; `accepted` is finalised later by the unary
// ReportTransferOutcome.

use core::{
    num::NonZeroU32,
    ops::ControlFlow,
    sync::atomic::{AtomicU64, Ordering},
};
use std::{collections::HashMap, sync::Arc};

use hardy_async::{TaskPool, sync::spin::Mutex};
use hardy_bpa::{
    async_trait,
    bpa::BpaRegistration,
    cla::{self, Cla, Error, ForwardBundleResult, TransferOutcome},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{bundle, eid::NodeId};
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::instrument;
use tracing::{debug, error, warn};

use super::{Bridge, Component, SinkSlot};
use crate::{
    cla::{
        AddPeerRequest, AddPeerResponse, ClaAddressType, DispatchMetadata, DispatchRequest,
        DispatchResponse, ForwardMetadata, ForwardRequest, ForwardResponse, Forwarding,
        Registration, RemovePeerRequest, RemovePeerResponse, ReportTransferOutcomeRequest,
        ReportTransferOutcomeResponse, SubscribeRequest, SubscribeResponse,
        cla_service_server::ClaService, dispatch_request, forward_request, forward_result,
        report_transfer_outcome_request, subscribe_request, subscribe_response,
    },
    error_status::embed_cla_error,
    server::{
        CHANNEL_DEPTH, DATA_CHANNEL_DEPTH, adapter,
        session::{Session, SessionStream},
    },
    stream::{self, Cancel, Chunk},
};

// The one point where BPA cla errors become gRPC statuses. The typed
// discriminator is embedded on the way out so the SDK can recover the
// exact variant past the coarse code.
fn cla_status(error: Error) -> Status {
    let status = match &error {
        Error::AlreadyExists(_) => Status::already_exists(error.to_string()),
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::StreamCancelled => Status::cancelled(error.to_string()),
        // Aligned with the services map: an under-delivering producer is
        // an invalid argument, an unaddressable declaration exhausts a
        // resource.
        Error::PayloadTooLarge { .. } | Error::PayloadUnaddressable { .. } => {
            Status::resource_exhausted(error.to_string())
        }
        Error::PayloadUnderrun { .. } => Status::invalid_argument(error.to_string()),
        // The internal chain may carry host detail an untrusted peer
        // must never see: log it server-side and ship a generic status.
        Error::Internal(e) => {
            error!("internal cla error: {e}");
            Status::internal("internal error")
        }
    };
    embed_cla_error(status, &error)
}

// -------------------------------------------------------------------
// The component as the BPA sees it
// -------------------------------------------------------------------

// The streams of the Forward call the CLA opens to execute an
// announced forwarding: the response sender the bundle chunks go down,
// and the request stream the result comes back up. The door hands
// these to the awaiting `forward_streamed`, which drives them inline.
struct ForwardCall {
    tx: Sender<Result<ForwardResponse, Status>>,
    requests: Streaming<ForwardRequest>,
}

// Removes its forwarding from the map when `forward` returns or its
// future is dropped, so a rendezvous is never leaked, however the
// call ends. The Forward door claims the entry with its own `remove`;
// this guard's later removal is then a harmless no-op.
//
// Removal checks the entry's sequence number: the dispatcher's
// single-in-flight claim can transiently break (a peer removal or a
// deferred Failed outcome racing an in-stream result releases the
// claim while this call still runs), and a stale guard must not
// remove a successor forwarding's live rendezvous.
struct ForwardingGuard<'a> {
    forwardings: &'a Mutex<HashMap<String, (u64, oneshot::Sender<ForwardCall>)>>,
    key: String,
    seq: u64,
}

impl Drop for ForwardingGuard<'_> {
    fn drop(&mut self) {
        let mut forwardings = self.forwardings.lock();
        if forwardings
            .get(&self.key)
            .is_some_and(|(seq, _)| *seq == self.seq)
        {
            forwardings.remove(&self.key);
        }
    }
}

struct GrpcCla {
    session: Session<SubscribeResponse>,
    sink: SinkSlot<dyn cla::Sink>,
    // The CLA's shape, declared at registration.
    address_type: Option<cla::ClaAddressType>,
    lane_count: Option<NonZeroU32>,
    // Announced forwardings awaiting their Forward call, keyed by the
    // wire's bundle id: the rendezvous the door answers. The sequence
    // number is the entry's identity, for the guard's conditional
    // removal.
    forwardings: Mutex<HashMap<String, (u64, oneshot::Sender<ForwardCall>)>>,
    forwarding_seq: AtomicU64,
}

impl GrpcCla {
    fn new(
        session: Session<SubscribeResponse>,
        address_type: Option<cla::ClaAddressType>,
        lane_count: Option<NonZeroU32>,
    ) -> Self {
        Self {
            session,
            sink: SinkSlot::new(),
            address_type,
            lane_count,
            forwardings: Mutex::new(HashMap::new()),
            forwarding_seq: AtomicU64::new(0),
        }
    }

    async fn event(&self, event: subscribe_response::Event) -> bool {
        self.session
            .event(SubscribeResponse { event: Some(event) })
            .await
    }
}

impl Component for GrpcCla {
    type Event = SubscribeResponse;

    fn session(&self) -> &Session<SubscribeResponse> {
        &self.session
    }

    async fn unregister_sink(&self) {
        if let Some(sink) = self.sink.peek() {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl Cla for GrpcCla {
    async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
        self.sink.set(sink);
    }

    async fn on_unregister(&self) {
        // BPA-initiated teardown: pull the trigger; the session task
        // catches it and runs the one exit sequence.
        self.session.abort();
    }

    fn address_type(&self) -> Option<cla::ClaAddressType> {
        self.address_type
    }

    fn lane_count(&self) -> Option<NonZeroU32> {
        self.lane_count
    }

    async fn forward(
        &self,
        lane: Option<u32>,
        cla_addr: &cla::ClaAddress,
        bundle_id: &bundle::Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        // Park the rendezvous first, then announce: the Forward door
        // answers by the announced id, so it must be reachable before
        // the client can learn of the forwarding. The guard removes it
        // on every exit, including a dropped future.
        let key = bundle_id.to_key();
        let (door_tx, door_rx) = oneshot::channel::<ForwardCall>();
        let seq = self.forwarding_seq.fetch_add(1, Ordering::Relaxed);
        self.forwardings.lock().insert(key.clone(), (seq, door_tx));
        let _guard = ForwardingGuard {
            forwardings: &self.forwardings,
            key: key.clone(),
            seq,
        };

        if !self
            .event(subscribe_response::Event::Forwarding(Forwarding {
                bundle_id: key,
                address: Some(cla_addr.clone().into()),
                lane,
                bundle_size: total_len,
            }))
            .await
        {
            return Err(cla::Error::Disconnected);
        }

        // Wait for the CLA to open its Forward call. The session dying
        // (or the door being dropped with it) leaves the bundle queued
        // in the BPA for a later registration.
        let cancelled = self.session.cancellation();
        let call = tokio::select! {
            biased;
            _ = cancelled.cancelled() => return Err(cla::Error::Disconnected),
            call = door_rx => match call {
                Ok(call) => call,
                Err(_) => return Err(cla::Error::Disconnected),
            },
        };

        // The door removed the rendezvous when it answered; the transfer
        // is driven inline so the borrowed BPA stream stays alive for
        // it: chunks stream down, the last one marked, until the result
        // that completes the forwarding arrives. Every ending without a
        // result fails the forwarding, so the BPA requeues the bundle:
        // a truncated BPA stream withdraws on the wire (the CLA must
        // not transmit a partial bundle); a cancel, half-close, or
        // failed request stream reports the transfer as cancelled, and
        // only genuine session death is disconnection.
        let ForwardCall { tx, mut requests } = call;
        let mut requests_open = true;
        loop {
            let segment = tokio::select! {
                biased;
                _ = cancelled.cancelled() => {
                    let _ = tx.try_send(Err(Status::aborted("Session closed")));
                    return Err(cla::Error::Disconnected);
                }
                // A result may arrive before the last chunk was pulled
                // (wire-legal); it still completes the forwarding.
                request = requests.message(), if requests_open => {
                    match on_forward_request(request, &mut requests_open, &tx) {
                        ControlFlow::Break(result) => return result,
                        ControlFlow::Continue(()) => continue,
                    }
                }
                segment = stream.recv() => segment,
            };
            let segment = match segment {
                Ok(segment) => segment,
                // The BPA's stream truncated: withdraw on the wire so the
                // CLA does not transmit a partial bundle, then fail so the
                // BPA requeues.
                Err(_) => {
                    let _ = tx.try_send(Ok(ForwardResponse::cancel()));
                    return Err(cla::Error::StreamCancelled);
                }
            };
            let last = matches!(segment, Segment::Final(_));

            for segment in stream::chunks(segment) {
                // Wait for send room, servicing the result and abandonment
                // while the CLA reads; a result mid-stream completes the
                // forwarding.
                let permit = loop {
                    tokio::select! {
                        biased;
                        _ = cancelled.cancelled() => {
                            let _ = tx.try_send(Err(Status::aborted("Session closed")));
                            return Err(cla::Error::Disconnected);
                        }
                        request = requests.message(), if requests_open => {
                            if let ControlFlow::Break(result) =
                                on_forward_request(request, &mut requests_open, &tx)
                            {
                                return result;
                            }
                        }
                        permit = tx.reserve() => {
                            let Ok(permit) = permit else {
                                // The client dropped the response stream.
                                return Err(cla::Error::StreamCancelled);
                            };
                            break permit;
                        }
                    }
                };
                permit.send(Ok(ForwardResponse::chunk(segment)));
            }

            if last {
                break;
            }
        }

        // The bundle is on the wire: wait for the CLA's result.
        loop {
            if !requests_open {
                let _ = tx.try_send(Err(Status::aborted("The call ended without a result")));
                return Err(cla::Error::StreamCancelled);
            }
            let request = tokio::select! {
                biased;
                _ = cancelled.cancelled() => {
                    let _ = tx.try_send(Err(Status::aborted("Session closed")));
                    return Err(cla::Error::Disconnected);
                }
                request = requests.message() => request,
            };
            if let ControlFlow::Break(result) = on_forward_request(request, &mut requests_open, &tx)
            {
                return result;
            }
        }
    }
}

// -------------------------------------------------------------------
// The doors' streaming halves
// -------------------------------------------------------------------

// One message from a Forward call's request side, its ending already
// translated: `Break` carries `forward`'s return value — a wire result
// completing the call, or the failure left after the terminal status
// was offered to the stream (best effort: a CLA that abandoned is not
// reading). Half-close and unexpected messages continue; the latter
// are not Debug-formatted, because a stray metadata message carries
// the session token, which must never reach the logs.
fn on_forward_request(
    request: Result<Option<ForwardRequest>, Status>,
    requests_open: &mut bool,
    tx: &Sender<Result<ForwardResponse, Status>>,
) -> ControlFlow<cla::Result<ForwardBundleResult>> {
    let status = match request {
        Ok(Some(ForwardRequest {
            request: Some(forward_request::Request::Result(result)),
        })) => match result.result {
            Some(forward_result::Result::Sent(_)) => {
                return ControlFlow::Break(Ok(ForwardBundleResult::Sent));
            }
            Some(forward_result::Result::NoNeighbour(_)) => {
                return ControlFlow::Break(Ok(ForwardBundleResult::NoNeighbour));
            }
            Some(forward_result::Result::Accepted(_)) => {
                return ControlFlow::Break(Ok(ForwardBundleResult::Accepted));
            }
            None => {
                warn!("Ignoring empty result");
                return ControlFlow::Continue(());
            }
        },
        Ok(Some(ForwardRequest {
            request: Some(forward_request::Request::Cancel(_)),
        })) => Status::cancelled("Forwarding abandoned"),
        Ok(Some(_)) => {
            warn!("Ignoring unexpected message on the Forward request side");
            return ControlFlow::Continue(());
        }
        Ok(None) => {
            *requests_open = false;
            return ControlFlow::Continue(());
        }
        Err(e) => {
            debug!("Forward stream failed: {e}");
            Status::aborted("Forward stream failed")
        }
    };
    let _ = tx.try_send(Err(status));
    ControlFlow::Break(Err(cla::Error::StreamCancelled))
}

// -------------------------------------------------------------------
// Control plane: the session
// -------------------------------------------------------------------

/// The CLA bridge. Shutting down the pool given to [`new`](Self::new)
/// tears the sessions and drives unregistration, so shut it down only
/// after the transport has stopped accepting.
#[derive(Clone)]
pub struct ClaServiceImpl {
    bridge: Bridge<GrpcCla>,
}

impl ClaServiceImpl {
    /// Bridges the convergence-layer surface of `bpa`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            bridge: Bridge::new(bpa, tasks),
        }
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
        let mut requests = request.into_inner();

        // The wire requires Register first, sent without waiting for
        // response headers, so the handshake runs inline and a failed
        // registration is a plain error on the call.
        let Some(subscribe_request::Request::Register(register)) =
            requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be Register",
            ));
        };
        let address_type = register
            .address_type
            .and_then(|t| ClaAddressType::try_from(t).ok())
            .and_then(Option::<cla::ClaAddressType>::from);
        // Bounded before it can size anything: the declared count later
        // drives a per-peer egress-queue allocation loop.
        let lane_count = match register.lane_count {
            Some(0) => return Err(Status::invalid_argument("lane_count must not be zero")),
            Some(n) if n > cla::MAX_LANE_COUNT => {
                return Err(Status::invalid_argument(format!(
                    "lane_count {n} exceeds the maximum of {}",
                    cla::MAX_LANE_COUNT
                )));
            }
            Some(n) => NonZeroU32::new(n),
            None => None,
        };

        // The token is minted before the BPA sees the component: the
        // session must be able to carry events the moment registration
        // completes. The JWT `sub` is the requested identity,
        // observability only.
        let token = self.bridge.sessions.mint(&format!("cla:{}", register.name));
        let (events_tx, events_rx) = mpsc::channel(CHANNEL_DEPTH);
        let cla = Arc::new(GrpcCla::new(
            Session::new(token.clone(), self.bridge.tasks.child_token(), events_tx),
            address_type,
            lane_count,
        ));

        let node_ids = self
            .bridge
            .bpa
            .register_cla(register.name, cla.clone(), None)
            .await
            .map_err(cla_status)?;

        // The stream yields the Registration first, by construction.
        let registration = SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(Registration {
                node_ids: node_ids.iter().map(ToString::to_string).collect(),
                session_token: token.into(),
            })),
        };
        Ok(Response::new(self.bridge.open_session(
            cla,
            registration,
            events_rx,
            requests,
            "cla_session",
        )))
    }

    // ---------------------------------------------------------------
    // Data plane: the doors
    // ---------------------------------------------------------------

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
        let cla = self.bridge.sessions.resolve(session_token)?;
        let peer_node = peer_node_id
            .map(|s| s.parse::<NodeId>())
            .transpose()
            .map_err(|e| Status::invalid_argument(format!("Invalid peer_node_id: {e}")))?;
        let peer_addr = peer_addr.map(cla::ClaAddress::try_from).transpose()?;

        // The BPA pulls the transfer chunk by chunk, parses and
        // validates the assembled bundle, and caps the reassembly with
        // its own bundle size limit.
        let mut reader = adapter::Reader::new(requests, cla.session.cancellation(), "Dispatch");
        match cla
            .sink
            .get()?
            .dispatch(peer_node.as_ref(), peer_addr.as_ref(), &mut reader)
            .await
        {
            Ok(()) => Ok(Response::new(DispatchResponse {})),
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
        let cla = self.bridge.sessions.resolve(session_token)?;

        // Claim the announced forwarding: removing the rendezvous makes
        // this call its sole executor, and a second Forward for the
        // same id finds nothing.
        let Some((_, door)) = cla.forwardings.lock().remove(&bundle_id) else {
            return Err(Status::not_found("No such forwarding"));
        };

        // `forward_streamed` drives the transfer through these streams, so
        // the BPA's bundle bytes never materialise in the bridge.
        let (tx, rx) = mpsc::channel(DATA_CHANNEL_DEPTH);
        if door.send(ForwardCall { tx, requests }).is_err() {
            // `forward_streamed` stopped awaiting between the announce
            // and now (session death); the forwarding is no longer live.
            return Err(Status::not_found("No such forwarding"));
        }

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    // ---------------------------------------------------------------
    // Peers and transfers
    // ---------------------------------------------------------------

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
        let cla = self.bridge.sessions.resolve(session_token)?;
        let address: cla::ClaAddress = address
            .ok_or_else(|| Status::invalid_argument("Missing address"))?
            .try_into()?;
        let node_ids = node_ids
            .into_iter()
            .map(|s| s.parse::<NodeId>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Status::invalid_argument(format!("Invalid node id: {e}")))?;

        let added = cla
            .sink
            .get()?
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
        let cla = self.bridge.sessions.resolve(session_token)?;
        let address: cla::ClaAddress = address
            .ok_or_else(|| Status::invalid_argument("Missing address"))?
            .try_into()?;

        let removed = cla
            .sink
            .get()?
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
        let cla = self.bridge.sessions.resolve(session_token)?;
        let bundle_id = bundle::Id::from_key(&bundle_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;
        let outcome = match outcome {
            Some(report_transfer_outcome_request::Outcome::Completed(_)) => {
                TransferOutcome::Completed
            }
            Some(report_transfer_outcome_request::Outcome::Failed(_)) => TransferOutcome::Failed,
            None => return Err(Status::invalid_argument("Missing outcome")),
        };

        cla.sink
            .get()?
            .transfer_outcome(&bundle_id, outcome)
            .await
            .map_err(cla_status)?;
        Ok(Response::new(ReportTransferOutcomeResponse {}))
    }
}

// The wire against a real BPA: the generated client, a port-0
// listener, and event-driven waits.
#[cfg(test)]
mod tests {
    // Only the client-gated mock CLAs declare lane counts.
    #[cfg(feature = "client")]
    use core::num::NonZeroU32;

    #[cfg(feature = "client")]
    use hardy_async::sync::spin::Once;
    use hardy_bpa::{Bytes, bpa::Bpa};
    use tonic::{
        Code,
        transport::{Channel, Server},
    };

    use super::{
        super::tests::{build_bpa, build_bundle, ipn1, serve, timeout, wait_torn_down},
        *,
    };
    use crate::cla::{
        ClaAddress, ForwardResult, Register, Unregister, cla_service_client::ClaServiceClient,
        cla_service_server::ClaServiceServer, forward_response,
    };
    use crate::server::session::Sessions;

    struct Harness {
        bpa: Arc<Bpa>,
        // Held live: dropping the pool would tear the sessions.
        #[expect(dead_code, reason = "held for its liveness")]
        tasks: TaskPool,
        client: ClaServiceClient<Channel>,
        #[cfg_attr(
            not(feature = "client"),
            expect(dead_code, reason = "read by the client SDK test")
        )]
        address: std::net::SocketAddr,
        // The session index, for the teardown barrier.
        sessions: Arc<Sessions<GrpcCla>>,
    }

    // A running BPA (node ipn:1) behind the bridge on a port-0
    // listener, plus a connected generated client.
    async fn harness() -> Harness {
        let bpa = build_bpa(ipn1(), false).await;

        let tasks = TaskPool::new();
        let service_impl = ClaServiceImpl::new(bpa.clone(), tasks.clone());
        let sessions = service_impl.bridge.sessions.clone();
        let service = ClaServiceServer::new(service_impl);
        let address = serve(Server::builder().add_service(service)).await;

        let client = ClaServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        Harness {
            bpa,
            tasks,
            client,
            address,
            sessions,
        }
    }

    struct Registered {
        requests: Sender<SubscribeRequest>,
        events: Streaming<SubscribeResponse>,
        node_ids: Vec<String>,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(client: &mut ClaServiceClient<Channel>, name: &str) -> Registered {
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: name.to_string(),
                    address_type: Some(ClaAddressType::Tcp.into()),
                    lane_count: None,
                })),
            })
            .await
            .unwrap();

        let mut events = client
            .subscribe(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let event = timeout(events.message()).await.unwrap().unwrap();
        let Some(subscribe_response::Event::Registration(registration)) = event.event else {
            panic!("expected the Registration event first, got {event:?}");
        };
        assert!(!registration.session_token.is_empty());

        Registered {
            requests,
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

    // A canonical BPv7 bundle as raw bytes, as received from a link.
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

    // Awaits the next Forwarding on the session stream. The announced
    // bundle is not byte-identical to the dispatched one: the BPA
    // rewrites it at egress (previous-node and hop-count blocks).
    async fn forwarding(registered: &mut Registered) -> Forwarding {
        let event = timeout(registered.events.message()).await.unwrap().unwrap();
        let Some(subscribe_response::Event::Forwarding(forwarding)) = event.event else {
            panic!("expected a Forwarding, got {event:?}");
        };
        forwarding
    }

    // Executes one Forward call: collects the streamed bundle, then
    // answers `result` (or abandons with an in-band cancel after the
    // first chunk when `abandon` is set).
    async fn execute_forward(
        client: &mut ClaServiceClient<Channel>,
        token: Bytes,
        bundle_id: &str,
        result: forward_result::Result,
        abandon: bool,
    ) -> Result<Vec<u8>, Status> {
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: token,
                    bundle_id: bundle_id.to_string(),
                })),
            })
            .await
            .unwrap();

        let mut stream = client.forward(ReceiverStream::new(rx)).await?.into_inner();
        let mut collected = Vec::new();
        loop {
            match stream.message().await?.and_then(|r| r.response) {
                Some(forward_response::Response::Chunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    if abandon {
                        requests
                            .send(ForwardRequest {
                                request: Some(forward_request::Request::Cancel(())),
                            })
                            .await
                            .unwrap();
                    }
                }
                Some(forward_response::Response::LastChunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    requests
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
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registration_returns_node_ids_and_a_token() {
        let mut harness = harness().await;

        let registered = register(&mut harness.client, "test-cla").await;
        assert_eq!(registered.node_ids.len(), 1);
        assert!(registered.node_ids[0].starts_with("ipn:1"));

        // A duplicate name is rejected as the local registry rejects it.
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "test-cla".to_string(),
                    address_type: None,
                    lane_count: None,
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .subscribe(ReceiverStream::new(rx))
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
        dispatch(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap();

        let forwarding = forwarding(&mut registered).await;
        assert_eq!(forwarding.address, Some(tcp_address(peer)));

        // Execute it: the streamed bundle is exactly the announced
        // size, carries the payload, and the result completes the
        // forwarding.
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

        let abandoned = execute_forward(
            &mut harness.client,
            registered.token.clone(),
            &announced.bundle_id,
            forward_result::Result::Sent(()),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(abandoned.code(), Code::Cancelled);

        // Abandoning fails the streamed forward, so the BPA requeues
        // the bundle and announces it again: the forwarding deferred,
        // not lost. The re-announcement streams the whole bundle and
        // completes it.
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

        // An outcome for a transfer no longer awaiting one is logged
        // and dropped, not an error.
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

        // A chunk but no last chunk: the half-close is a truncation,
        // not a commit.
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

        // The BPA shutdown joins its worker pool, so anything it would
        // announce has been announced and the session stream then ends;
        // draining it to that end must surface no Forwarding.
        harness.bpa.shutdown().await;
        while let Some(event) = registered.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Forwarding(_))),
                "a truncated dispatch must not forward"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_dispatch_is_discarded() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;

        // A live peer for the destination, so a committed bundle would
        // surface as a Forwarding within the timeout below.
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

        // Discarded means discarded: nothing was committed. The BPA
        // shutdown joins its worker pool and ends the session stream, so
        // draining it to that end must surface no Forwarding for the
        // route that exists.
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

        // The client vanishes without Unregister: dropping the rpc's
        // streams is caught by the response-stream guard and the request
        // half-close, and the session tears down. The teardown signal
        // fires once the token is gone AND the CLA is unregistered from
        // the BPA, so both the rejection and the re-registration below
        // are race-free.
        let mut torn = harness.sessions.torn_down();
        drop(registered.events);
        drop(registered.requests);
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

        // Teardown also unregistered the CLA from the BPA, so the name is
        // free for a new registration, which now succeeds on the first try.
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "test-cla".to_string(),
                    address_type: None,
                    lane_count: None,
                })),
            })
            .await
            .unwrap();
        harness
            .client
            .subscribe(ReceiverStream::new(rx))
            .await
            .unwrap();

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unregister_ends_the_session_and_invalidates_the_token() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, "test-cla").await;
        let mut torn = harness.sessions.torn_down();

        registered
            .requests
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

        // The token dies in the session task's teardown, which runs
        // after the stream closes; wait for the teardown signal, so the
        // rejection below is asserted without a race.
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

        // No forwarding was announced, so any Forward call finds no
        // parked rendezvous.
        let (requests, rx) = mpsc::channel(4);
        requests
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
            .forward(ReceiverStream::new(rx))
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
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(ForwardRequest {
                request: Some(forward_request::Request::Result(ForwardResult {
                    result: Some(forward_result::Result::Sent(())),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .forward(ReceiverStream::new(rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);

        drop(registered);
        harness.bpa.shutdown().await;
    }

    // The Forward door's claim is single-executor: with a live call
    // holding the announced forwarding, a duplicate Forward for the
    // same id answers NOT_FOUND, and the live call completes untouched.
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

        // Two wire chunks down, so the first call can hold the
        // forwarding open after reading one.
        let payload = vec![0x5a; crate::CHUNK_SIZE + 3];
        let bundle = build_bundle("ipn:3.1", "ipn:2.1", &payload);
        dispatch(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap();
        let announced = forwarding(&mut registered).await;

        // Open the first Forward and read exactly one chunk.
        let (requests, rx) = mpsc::channel(4);
        requests
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
            .forward(ReceiverStream::new(rx))
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
        requests
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

    // The declared lane count is validated at the wire boundary: zero
    // and above-the-bound declarations are INVALID_ARGUMENT, and the
    // bound itself registers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lane_count_is_validated_at_registration() {
        let mut harness = harness().await;

        for lane_count in [Some(0), Some(cla::MAX_LANE_COUNT + 1)] {
            let (requests, rx) = mpsc::channel(4);
            requests
                .send(SubscribeRequest {
                    request: Some(subscribe_request::Request::Register(Register {
                        name: "bad-lanes".to_string(),
                        address_type: None,
                        lane_count,
                    })),
                })
                .await
                .unwrap();
            let status = harness
                .client
                .subscribe(ReceiverStream::new(rx))
                .await
                .unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument);
        }

        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "max-lanes".to_string(),
                    address_type: None,
                    lane_count: Some(cla::MAX_LANE_COUNT),
                })),
            })
            .await
            .unwrap();
        let mut events = harness
            .client
            .subscribe(ReceiverStream::new(rx))
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

    // A CLA answering Accepted owns the transfer: the deferred
    // Completed outcome, reported through the SDK sink, resolves the
    // bundle terminally, and nothing is re-offered.
    #[cfg(feature = "client")]
    struct AcceptingCla {
        sink: Once<Box<dyn cla::Sink>>,
        forwarded: mpsc::Sender<bundle::Id>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl Cla for AcceptingCla {
        async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        fn lane_count(&self) -> Option<NonZeroU32> {
            None
        }

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            bundle_id: &bundle::Id,
            _total_len: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<ForwardBundleResult> {
            hardy_bpa::stream::concat_stream(stream, usize::MAX, None)
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
        let remote = crate::client::BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let (forwarded_tx, mut forwarded_rx) = mpsc::channel(4);
        let cla = Arc::new(AcceptingCla {
            sink: Once::new(),
            forwarded: forwarded_tx,
        });
        remote
            .register_cla("sdk-accepting".to_string(), cla.clone())
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

        // Completed means resolved: no re-offer follows. The shutdown
        // joins the BPA worker pool, so any re-offer it would make has
        // been made and the SDK's session ends, dropping its CLA handle;
        // dropping the test's handle then closes the forward channel, and
        // draining it to its end must find no re-offer.
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

    // The SDK refuses to register a CLA whose declared lane count
    // exceeds the bound, rather than silently clamping it: a clamped
    // registration would leave the CLA believing in lanes the BPA
    // never offers.
    #[cfg(feature = "client")]
    struct OverLanedCla;

    #[cfg(feature = "client")]
    #[async_trait]
    impl Cla for OverLanedCla {
        async fn on_register(&self, _sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {}

        async fn on_unregister(&self) {}

        fn lane_count(&self) -> Option<NonZeroU32> {
            NonZeroU32::new(cla::MAX_LANE_COUNT + 1)
        }

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            _bundle_id: &bundle::Id,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<ForwardBundleResult> {
            Ok(ForwardBundleResult::Sent)
        }
    }

    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sdk_rejects_an_over_declared_lane_count() {
        let harness = harness().await;
        let remote = crate::client::BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let error = remote
            .register_cla("over-laned".to_string(), Arc::new(OverLanedCla))
            .await
            .unwrap_err();
        assert!(
            matches!(error, cla::Error::Internal(_)),
            "expected the SDK to reject the declaration, got {error:?}"
        );

        harness.bpa.shutdown().await;
    }

    // A CLA behind the client SDK: it announces a peer, dispatches a
    // bundle received from the link, and the BPA forwards it back out
    // through the CLA's `forward` (via the default `forward_streamed`).
    #[cfg(feature = "client")]
    struct SdkCla {
        sink: Once<Box<dyn cla::Sink>>,
        forwarded: mpsc::Sender<Bytes>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl Cla for SdkCla {
        async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        fn address_type(&self) -> Option<cla::ClaAddressType> {
            Some(cla::ClaAddressType::Tcp)
        }

        fn lane_count(&self) -> Option<NonZeroU32> {
            None
        }

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            _bundle_id: &bundle::Id,
            _total_len: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<ForwardBundleResult> {
            let bundle = hardy_bpa::stream::concat_stream(stream, usize::MAX, None)
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
        let remote =
            crate::client::BpaClient::new(format!("http://{}", harness.address), TaskPool::new())
                .unwrap();

        let (forwarded_tx, mut forwarded_rx) = mpsc::channel(4);
        let cla = Arc::new(SdkCla {
            sink: Once::new(),
            forwarded: forwarded_tx,
        });
        let node_ids = remote
            .register_cla("sdk-cla".to_string(), cla.clone())
            .await
            .unwrap();
        assert!(!node_ids.is_empty());

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

        // The BPA rewrites at egress (previous-node, hop-count), so the
        // forwarded bytes are not byte-identical, but the payload
        // survives.
        let forwarded = timeout(forwarded_rx.recv()).await.unwrap();
        assert!(forwarded.windows(payload.len()).any(|w| w == payload));

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }
}
