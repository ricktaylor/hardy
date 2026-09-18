// The CLA surface: the `hardy.cla.v1` wire served against the
// convergence-layer surface of a BPA. Two shapes differ from the
// template: Dispatch streams straight into the BPA through
// `Sink::dispatch`, and Forward runs the same announce-and-collect
// exchange as a delivery but ends on the CLA's result, which may
// arrive at any point of the transfer, with only `no_neighbour`
// admitted before the last chunk. An `accepted` result is finalised
// later by the unary ReportTransferOutcome.

use core::{
    num::{NonZeroU32, NonZeroU64},
    ops::ControlFlow,
    pin::pin,
};
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::{
    async_trait,
    bpa::BpaRegistration,
    cla::{self, Cla, Error, ForwardBundleResult, TransferOutcome},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{bundle::Id as BundleId, eid::NodeId};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::instrument;
use tracing::{debug, error, warn};

use crate::{
    MAX_LANE_COUNT,
    cla::{
        Acceptance, AddPeerRequest, AddPeerResponse, ClaAddressType, DispatchMetadata,
        DispatchRequest, DispatchResponse, ForwardMetadata, ForwardRequest, ForwardResponse,
        Forwarding, Register, Registration, RemovePeerRequest, RemovePeerResponse,
        ReportTransferOutcomeRequest, ReportTransferOutcomeResponse, SubscribeRequest,
        SubscribeResponse, cla_service_server::ClaService, dispatch_request, forward_request,
        forward_result, report_transfer_outcome_request, subscribe_request, subscribe_response,
    },
    server::{
        DATA_CHANNEL_DEPTH, SessionError,
        adapter::{Interrupted, RequestReader, ResponseWriter},
        announce::{Announcements, Collection},
        session::{Session, SessionStream},
        slot::Slot,
        subscribe::{Sessions, SubscribeHandler},
    },
    status::embed_cla_error,
    token::Token,
    transfer::Writer,
};

// The one point where BPA cla errors become gRPC statuses. The typed
// discriminator is embedded so the SDK can recover the exact variant
// past the coarse code.
fn cla_status(error: Error) -> Status {
    let status = match &error {
        Error::AlreadyExists(_) => Status::already_exists(error.to_string()),
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::StreamCancelled => Status::cancelled(error.to_string()),
        // Aligned with the services map: an under-delivering producer
        // is an invalid argument, an unaddressable declaration exhausts
        // a resource.
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

// The component as the BPA sees it.
// What registering produced: the sink the doors call, and the size cap
// the BPA negotiated. Published together because they arrive together,
// in the one `on_register` call.
#[derive(Clone)]
struct Registered {
    sink: Arc<dyn cla::Sink>,
    max_bundle_size: Option<NonZeroU64>,
}

struct GrpcCla {
    session: Session<SubscribeResponse>,
    // Set by `on_register`, which is awaited inside `register_cla`, so
    // it is readable before registration returns.
    registered: Slot<Registered>,
    // Announced forwardings awaiting their Forward call.
    forwardings: Announcements<ForwardResponse, ForwardRequest>,
}

impl GrpcCla {
    fn new(session: Session<SubscribeResponse>) -> Self {
        Self {
            session,
            registered: Slot::new(),
            forwardings: Announcements::default(),
        }
    }

    // One event down the session stream. A torn-down session drops it,
    // which to the BPA is this component's disconnection.
    async fn event(&self, event: subscribe_response::Event) -> cla::Result<()> {
        self.session
            .event(SubscribeResponse { event: Some(event) })
            .await
            .map_err(|_| cla::Error::Disconnected)
    }
}

impl SubscribeHandler for GrpcCla {
    type Event = SubscribeResponse;
    type Request = SubscribeRequest;
    type Register = Register;

    const LABEL: &'static str = "cla";

    fn session(&self) -> &Session<SubscribeResponse> {
        &self.session
    }

    fn into_register(request: SubscribeRequest) -> Option<Register> {
        let Some(subscribe_request::Request::Register(register)) = request.request else {
            return None;
        };
        Some(register)
    }

    async fn register(
        bpa: &Arc<dyn BpaRegistration>,
        register: Register,
        cancel: CancellationToken,
        events_tx: mpsc::Sender<Result<SubscribeResponse, Status>>,
    ) -> Result<(Arc<Self>, SubscribeResponse), Status> {
        let address_type = register
            .address_type
            .and_then(|t| ClaAddressType::try_from(t).ok())
            .and_then(Option::<cla::ClaAddressType>::from);
        // Bounded before it can size anything: the declared count later
        // drives a per-peer egress-queue allocation loop.
        let lane_count = match register.lane_count {
            Some(0) => return Err(Status::invalid_argument("lane_count must not be zero")),
            Some(n) if n > MAX_LANE_COUNT => {
                return Err(Status::invalid_argument(format!(
                    "lane_count {n} exceeds the maximum of {MAX_LANE_COUNT}"
                )));
            }
            Some(n) => NonZeroU32::new(n),
            None => None,
        };
        let max_bundle_size = match register.max_bundle_size {
            Some(0) => {
                return Err(Status::invalid_argument("max_bundle_size must not be zero"));
            }
            Some(n) => NonZeroU64::new(n),
            None => None,
        };

        // Minted before the BPA sees the component: the session must be
        // able to carry events the moment registration completes.
        let token = Token::mint(&format!("{}:{}", Self::LABEL, register.name));
        let cla = Arc::new(GrpcCla::new(Session::new(token.clone(), cancel, events_tx)));

        let node_ids = bpa
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
            .map_err(cla_status)?;

        // The cap the client is told is the negotiated one, read back
        // from what `on_register` published, not the one it declared.
        let negotiated = cla
            .registered
            .get()
            .and_then(|registered| registered.max_bundle_size)
            .map(NonZeroU64::get);

        Ok((
            cla,
            SubscribeResponse {
                event: Some(subscribe_response::Event::Registration(Registration {
                    node_ids: node_ids.iter().map(ToString::to_string).collect(),
                    session_token: token.into(),
                    max_bundle_size: negotiated,
                })),
            },
        ))
    }

    async fn on_request(&self, request: SubscribeRequest) -> ControlFlow<()> {
        match request.request {
            // The one message that ends a subscription client-side.
            Some(subscribe_request::Request::Unregister(_)) => ControlFlow::Break(()),
            // Not Debug-formatted: the message carries the session
            // token, which must never reach the logs.
            Some(subscribe_request::Request::Register(_)) | None => {
                warn!("Ignoring unexpected message on the session stream");
                ControlFlow::Continue(())
            }
        }
    }

    async fn unregister(&self) {
        if let Some(registered) = self.registered.get() {
            registered.sink.unregister().await;
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
        self.registered.set(Registered {
            sink: Arc::from(sink),
            max_bundle_size,
        });
    }

    async fn on_unregister(&self) {
        // The session task catches this and runs the one exit sequence.
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
        // Recorded before the Forwarding event carries the id to the
        // client, so the Forward door can always find it. Held for the
        // whole exchange: it withdraws on any ending short of the door's
        // collect, sparing a successor's live entry under the same id.
        let mut announced = self.forwardings.announce(bundle_id);

        self.event(subscribe_response::Event::Forwarding(Forwarding {
            bundle_id: bundle_id.to_key(),
            address: Some(cla_addr.clone().into()),
            lane,
            bundle_size: total_len,
        }))
        .await?;

        // Every ending here leaves the bundle queued in the BPA for a
        // later collection; only session death is this component's
        // disconnection.
        let cancelled = self.session.cancellation();
        let Collection {
            responses_tx,
            requests,
        } = announced.collected(&cancelled).await.map_err(|e| match e {
            SessionError::Closed => cla::Error::Disconnected,
            SessionError::Superseded => cla::Error::StreamCancelled,
        })?;

        // Driven inline so the borrowed BPA stream stays alive for it.
        // Every ending without a result fails the forwarding, so the BPA
        // requeues the bundle; only session death is disconnection.
        let writer = ResponseWriter::new(&responses_tx, &cancelled, stream);
        let mut result = pin!(forwarding_result(requests));
        let answer = tokio::select! {
            biased;
            // What a CLA may say before the last chunk is narrower.
            // Losing this race drops the writer, so the rest of the
            // bundle is never pulled.
            early = &mut result => early_result(early),
            pushed = writer.write_all() => match pushed {
                // The whole bundle is on the wire: the CLA's result is
                // the only thing left that can complete the forwarding.
                ControlFlow::Continue(()) => tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => {
                        let _ = responses_tx.try_send(Err(Status::unavailable("Session closed")));
                        return Err(cla::Error::Disconnected);
                    }
                    answered = result => answered,
                },
                ControlFlow::Break(Interrupted::Session) => return Err(cla::Error::Disconnected),
                // A truncated BPA stream has already withdrawn on the
                // wire, so the CLA does not transmit a partial bundle.
                ControlFlow::Break(Interrupted::Broken) => return Err(cla::Error::StreamCancelled),
            },
        };

        match answer {
            Ok(result) => Ok(result),
            // Awaited, not `try_send`: with the buffer full a dropped
            // status reaches the CLA as a clean end-of-stream, which it
            // reports as truncation instead of the real cause. The
            // cancel bounds the wait, so a CLA that stopped draining
            // cannot wedge this call.
            Err(status) => {
                tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => {}
                    _ = responses_tx.send(Err(status)) => {}
                }
                Err(cla::Error::StreamCancelled)
            }
        }
    }
}

// The CLA's answer on a Forward call's request side: a `result`
// completes the forwarding, everything else fails it with the status
// the call should end with. A half-closed request side can never carry
// a result, so it fails there rather than wait for one that cannot come.
//
// The announcer races this against the transfer, because the result may
// arrive at any point of one and always ends it, then awaits the same
// future once the last chunk is on the wire. Which results are admitted
// before the last chunk is the announcer's to police, since only it
// knows which phase the answer arrived in.
async fn forwarding_result(
    mut requests: Streaming<ForwardRequest>,
) -> Result<ForwardBundleResult, Status> {
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
                None => warn!("Ignoring empty result"),
            },
            Ok(Some(ForwardRequest {
                request: Some(forward_request::Request::Cancel(_)),
            })) => return Err(Status::cancelled("Forwarding abandoned")),
            // Not Debug-formatted: a stray metadata message carries the
            // session token, which must never reach the logs.
            Ok(Some(_)) => warn!("Ignoring unexpected message on the Forward request side"),
            Ok(None) => return Err(Status::aborted("The call ended without a result")),
            Err(e) => {
                debug!("Forward stream failed: {e}");
                return Err(Status::aborted("Forward stream failed"));
            }
        }
    }
}

// What a result that arrived before the last chunk is worth. A
// neighbour can go away at any point, so `no_neighbour` is answerable
// throughout; `sent` and `accepted` are claims about bytes the CLA has
// not been given, and honouring one would resolve a bundle it provably
// does not hold.
fn early_result(
    answer: Result<ForwardBundleResult, Status>,
) -> Result<ForwardBundleResult, Status> {
    match answer {
        Ok(ForwardBundleResult::NoNeighbour) => Ok(ForwardBundleResult::NoNeighbour),
        Ok(ForwardBundleResult::Sent | ForwardBundleResult::Accepted) => {
            Err(Status::invalid_argument("Result before the final chunk"))
        }
        Err(status) => Err(status),
    }
}

/// The CLA surface. Shutting down the pool given to [`new`](Self::new)
/// tears the subscriptions and drives unregistration, so shut it down
/// only after the transport has stopped accepting.
#[derive(Clone)]
pub struct ClaServiceImpl {
    sessions: Sessions<GrpcCla>,
}

impl ClaServiceImpl {
    /// Serves the convergence-layer surface of `bpa`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            sessions: Sessions::new(bpa, tasks),
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
        self.sessions.subscribe(request.into_inner()).await
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
        let cla = self.sessions.resolve(session_token)?;
        let peer_node = peer_node_id
            .map(|s| s.parse::<NodeId>())
            .transpose()
            .map_err(|e| Status::invalid_argument(format!("Invalid peer_node_id: {e}")))?;
        let peer_addr = peer_addr.map(cla::ClaAddress::try_from).transpose()?;

        // The BPA pulls chunk by chunk and validates the assembled
        // bundle under its own size cap.
        let mut reader = RequestReader::new(requests, cla.session.cancellation(), "Dispatch");
        match cla
            .registered
            .get()
            .ok_or_else(|| cla_status(Error::Disconnected))?
            .sink
            .dispatch(peer_node.as_ref(), peer_addr.as_ref(), &mut reader)
            .await
        {
            // Today's BPA reports acceptance as `Ok`, so a refusal
            // still arrives as a status: see
            // [`TODO.md`](../../../docs/TODO.md).
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
        let cla = self.sessions.resolve(session_token)?;
        let bundle_id =
            BundleId::from_key(&bundle_id).map_err(|_| Status::not_found("No such forwarding"))?;

        // A single-use take, so this call is the forwarding's sole
        // executor and a second Forward for the same id finds nothing.
        let Some(call_tx) = cla.forwardings.collect(&bundle_id) else {
            return Err(Status::not_found("No such forwarding"));
        };

        // `forward` drives the transfer through these streams, so
        // the BPA's bundle bytes never materialise in the server.
        let (responses_tx, responses_rx) = mpsc::channel(DATA_CHANNEL_DEPTH);
        if call_tx
            .send(Collection {
                responses_tx,
                requests,
            })
            .is_err()
        {
            // `forward` stopped awaiting between the announce
            // and now (session death); the forwarding is no longer live.
            return Err(Status::not_found("No such forwarding"));
        }

        Ok(Response::new(ReceiverStream::new(responses_rx)))
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
        let cla = self.sessions.resolve(session_token)?;
        let address: cla::ClaAddress = address
            .ok_or_else(|| Status::invalid_argument("Missing address"))?
            .try_into()?;
        let node_ids = node_ids
            .into_iter()
            .map(|s| s.parse::<NodeId>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Status::invalid_argument(format!("Invalid node id: {e}")))?;

        let added = cla
            .registered
            .get()
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
        let cla = self.sessions.resolve(session_token)?;
        let address: cla::ClaAddress = address
            .ok_or_else(|| Status::invalid_argument("Missing address"))?
            .try_into()?;

        let removed = cla
            .registered
            .get()
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
        let cla = self.sessions.resolve(session_token)?;
        let bundle_id = BundleId::from_key(&bundle_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;
        let outcome = match outcome {
            Some(report_transfer_outcome_request::Outcome::Completed(_)) => {
                TransferOutcome::Completed
            }
            Some(report_transfer_outcome_request::Outcome::Failed(_)) => TransferOutcome::Failed,
            None => return Err(Status::invalid_argument("Missing outcome")),
        };

        cla.registered
            .get()
            .ok_or_else(|| cla_status(Error::Disconnected))?
            .sink
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
    // Only the client-gated mock CLAs declare lane counts and receive
    // the negotiated cap.
    #[cfg(feature = "client")]
    use core::num::{NonZeroU32, NonZeroU64};

    use std::net::SocketAddr;

    #[cfg(feature = "client")]
    use crate::client::BpaClient;
    #[cfg(feature = "client")]
    use hardy_async::sync::spin::Once;
    #[cfg(feature = "client")]
    use hardy_bpa::stream::concat_stream;
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
    use crate::server::subscribe::Sessions;

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
        address: SocketAddr,
        // The session index, for the teardown barrier.
        sessions: Sessions<GrpcCla>,
    }

    // A running BPA (node ipn:1) behind the surface on a port-0
    // listener, plus a connected generated client.
    async fn harness() -> Harness {
        let bpa = build_bpa(ipn1(), false).await;

        let tasks = TaskPool::new();
        let service_impl = ClaServiceImpl::new(bpa.clone(), tasks.clone());
        let sessions = service_impl.sessions.clone();
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

    // Awaits the next Forwarding. The announced bundle is not
    // byte-identical to the dispatched one: the BPA rewrites it at
    // egress.
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
        loop {
            match stream.message().await?.and_then(|r| r.response) {
                Some(forward_response::Response::Chunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    if abandon {
                        requests_tx
                            .send(ForwardRequest {
                                request: Some(forward_request::Request::Cancel(())),
                            })
                            .await
                            .unwrap();
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

    // What a CLA may answer before it holds the whole bundle: the
    // neighbour going away is answerable throughout, a `sent` or
    // `accepted` it cannot have earned is refused.
    #[test]
    fn an_early_result_admits_only_no_neighbour() {
        assert!(matches!(
            early_result(Ok(ForwardBundleResult::NoNeighbour)),
            Ok(ForwardBundleResult::NoNeighbour)
        ));

        for claimed in [ForwardBundleResult::Sent, ForwardBundleResult::Accepted] {
            let Err(status) = early_result(Ok(claimed)) else {
                panic!("a result the CLA cannot have earned must be refused");
            };
            assert_eq!(status.code(), Code::InvalidArgument);
        }

        let Err(status) = early_result(Err(Status::cancelled("Forwarding abandoned"))) else {
            panic!("an abandonment must keep its own status");
        };
        assert_eq!(status.code(), Code::Cancelled);
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

        // Abandoning fails the forward, so the BPA requeues the bundle
        // and announces it again: deferred, not lost.
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

    #[ignore = "the BPA commits a cancelled dispatch until the final-segment gate lands: see docs/TODO.md"]
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

        // Discarded means discarded. The shutdown joins the BPA's
        // worker pool and ends the session stream, so draining it must
        // surface no Forwarding for the route that exists.
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
        // fires once the token is gone and the CLA unregistered, so both
        // the rejection and the re-registration below are race-free.
        let mut torn = harness.sessions.torn_down();
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

        // Teardown also unregistered the CLA from the BPA, so the name is
        // free for a new registration, which now succeeds on the first try.
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
        let mut torn = harness.sessions.torn_down();

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

        // Teardown runs after the stream closes, so the signal is what
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

        // No forwarding was announced, so any Forward call finds no
        // parked rendezvous.
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

        // Two wire chunks down, so the first call can hold the
        // forwarding open after reading one.
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

    // A CLA answering Accepted owns the transfer: the deferred
    // Completed outcome resolves the bundle terminally.
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

        // Completed means resolved. The shutdown joins the BPA's worker
        // pool and ends the SDK session, so draining the forward channel
        // to its end must find no re-offer.
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

    // The SDK refuses an over-declared lane count rather than clamp it:
    // a clamped registration would leave the CLA believing in lanes the
    // BPA never offers.
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

    // A CLA behind the client SDK announces a peer, dispatches a bundle
    // from the link, and the BPA forwards it back out through `forward`.
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
