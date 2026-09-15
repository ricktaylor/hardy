//! The CLA session.

use core::{
    num::{NonZeroU32, NonZeroU64},
    ops::ControlFlow,
    pin::pin,
};
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::{
    async_trait,
    cla::{self, Cla, ClaInit, Error, ForwardBundleResult, Result, Sink, TransferOutcome},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{bundle::Id as BundleId, eid::NodeId};
use tokio::{
    select,
    sync::mpsc::{self, Sender, WeakSender, channel},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Status, Streaming, transport::Channel};
use tracing::warn;

use super::{expect_registration, next_event};
use crate::{
    MAX_LANE_COUNT, MAX_MESSAGE_SIZE,
    chunking::{ChunkReceiver, ChunkSender, MaxChunkSize},
    cla::{
        AddPeerRequest, AddressError, ClaAddress, ClaAddressType, DispatchMetadata,
        DispatchRequest, ForwardMetadata, ForwardRequest, ForwardResult, Forwarding,
        LaneCountError, Register, RemovePeerRequest, ReportTransferOutcomeRequest,
        SubscribeRequest, SubscribeResponse, cla_service_client::ClaServiceClient,
        dispatch_request, forward_request, forward_result, report_transfer_outcome_request,
        subscribe_request, subscribe_response,
    },
    client::services::{SUBSCRIBE_REQUEST_CAPACITY, TRANSFER_REQUEST_CAPACITY},
    grammar::{Cancel, Unregister},
    token::Token,
};

/// Maps a status from the registration handshake to the CLA's error.
///
/// As [`session_error`], except that `ALREADY_EXISTS` is `name` held by
/// another session. The caller passes the name because the status does not
/// carry it: the client asked for it, so it already knows which one was
/// refused.
fn registration_error(status: Status, name: &str) -> Error {
    if status.code() == Code::AlreadyExists {
        Error::AlreadyExists(name.to_string())
    } else {
        session_error(status)
    }
}

/// Maps the status a session stream or a unary call ends with to the CLA's
/// error.
///
/// The status is a gRPC code and a message, so this reads the code: a code the
/// server only ever sends when it has ended the session is `Disconnected`, and
/// anything else, which includes a transport failure, is `Internal`, which
/// keeps the status and so its message.
fn session_error(status: Status) -> Error {
    match status.code() {
        Code::Unauthenticated | Code::Unavailable | Code::DeadlineExceeded => Error::Disconnected,
        _ => Error::Internal(status.into()),
    }
}

/// The [`Sink`] a CLA receives from [`ClaSession::subscribe`].
///
/// `dispatch` is a `Dispatch` call, whose response carries the verdict;
/// anything but an explicit acceptance, including a call that fails for that
/// one transfer, is [`Refused`](cla::Acceptance::Refused), so the CLA never
/// acknowledges to its peer a bundle the BPA did not confirm. `add_peer`,
/// `remove_peer` and `transfer_outcome` are the unary calls of the same names,
/// and `unregister` is an `Unregister` on the session stream.
pub struct GrpcClaSink {
    client: ClaServiceClient<Channel>,
    token: Token,
    max_chunk_size: MaxChunkSize,
    requests_tx: Sender<SubscribeRequest>,
}

#[async_trait]
impl Sink for GrpcClaSink {
    async fn unregister(&self) {
        let _ = self.requests_tx.send(SubscribeRequest::unregister()).await;
    }

    async fn dispatch(
        &self,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&cla::ClaAddress>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<cla::Acceptance> {
        let metadata = DispatchRequest {
            request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                session_token: self.token.clone().into(),
                peer_node_id: peer_node.map(ToString::to_string),
                peer_address: peer_addr.cloned().map(ClaAddress::from),
                // The BPA's receive door announces no size, so the server sees
                // an undeclared transfer.
                bundle_size: None,
            })),
        };
        let mut client = self.client.clone();
        let (requests_tx, requests_rx) = channel(TRANSFER_REQUEST_CAPACITY);
        let sender = ChunkSender::new(requests_tx, stream, self.max_chunk_size);
        // The call runs against the writing, because the server may answer
        // before the stream is finished and the rest of it then has nowhere
        // to go.
        let mut call = pin!(client.dispatch(ReceiverStream::new(requests_rx)));
        let response = select! {
            biased;
            response = &mut call => response,
            () = sender.send_all(metadata) => call.await,
        };

        match response {
            Ok(response) => Ok(response.into_inner().acceptance().into()),
            // A call that fails carries no verdict, and no verdict means
            // refusal, so a code the server sends for one transfer, a
            // cancelled or truncated stream or a bundle over a size cap, is
            // `Refused`: the CLA withholds its acknowledgement and carries
            // on, as the `Sink` contract has it do for a transfer the BPA did
            // not take. A code the server sends only when the session is over
            // is `Disconnected`, and anything else is `Internal`.
            Err(status) => match status.code() {
                Code::Cancelled | Code::Aborted | Code::ResourceExhausted => {
                    Ok(cla::Acceptance::Refused)
                }
                _ => Err(session_error(status)),
            },
        }
    }

    async fn add_peer(&self, cla_addr: cla::ClaAddress, node_ids: &[NodeId]) -> Result<bool> {
        let response = self
            .client
            .clone()
            .add_peer(AddPeerRequest {
                session_token: self.token.clone().into(),
                node_ids: node_ids.iter().map(ToString::to_string).collect(),
                address: Some(cla_addr.into()),
            })
            .await
            .map_err(session_error)?
            .into_inner();
        Ok(response.added)
    }
    async fn remove_peer(&self, cla_addr: &cla::ClaAddress) -> Result<bool> {
        let response = self
            .client
            .clone()
            .remove_peer(RemovePeerRequest {
                session_token: self.token.clone().into(),
                address: Some(cla_addr.clone().into()),
            })
            .await
            .map_err(session_error)?
            .into_inner();
        Ok(response.removed)
    }

    async fn transfer_outcome(&self, bundle_id: &BundleId, outcome: TransferOutcome) -> Result<()> {
        let outcome = match outcome {
            TransferOutcome::Completed => report_transfer_outcome_request::Outcome::Completed(()),
            TransferOutcome::Failed => report_transfer_outcome_request::Outcome::Failed(()),
        };
        self.client
            .clone()
            .report_transfer_outcome(ReportTransferOutcomeRequest {
                session_token: self.token.clone().into(),
                bundle_id: bundle_id.to_key(),
                outcome: Some(outcome),
            })
            .await
            .map_err(session_error)?;
        Ok(())
    }
}

/// The state a session's tasks share: the CLA, a client for its data-plane
/// calls, its token, and the token that cancels them all.
struct ClaSessionContext {
    cla: Arc<dyn Cla>,
    client: ClaServiceClient<Channel>,
    token: Token,
    cancel: CancellationToken,
}

impl ClaSessionContext {
    fn new(
        cla: Arc<dyn Cla>,
        client: ClaServiceClient<Channel>,
        token: Token,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            cla,
            client,
            token,
            cancel: cancel.child_token(),
        })
    }

    /// Executes one announced forwarding.
    ///
    /// Opens a `Forward` call for the bundle, hands its response stream to the
    /// CLA's `forward` as the bundle's source, and sends the CLA's result on the
    /// request stream, after which the response stream is read to its end so the
    /// result cannot be lost in transit. A malformed bundle id or address in the
    /// announcement, or an error from the CLA, sends a `cancel` instead, so the BPA
    /// keeps the bundle. Session cancellation ends the stream the CLA is reading,
    /// so `forward` returns on its own terms, and the BPA then keeps the bundle
    /// too.
    async fn forward(self: Arc<Self>, forwarding: Forwarding) {
        let Forwarding {
            bundle_id,
            address,
            lane,
            bundle_size,
        } = forwarding;
        let mut client = self.client.clone();

        let (requests_tx, requests_rx) = mpsc::channel::<ForwardRequest>(TRANSFER_REQUEST_CAPACITY);
        if requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: self.token.clone().into(),
                    bundle_id: bundle_id.clone(),
                })),
            })
            .await
            .is_err()
        {
            return;
        }
        let mut responses = tokio::select! {
            biased;
            result = client.forward(ReceiverStream::new(requests_rx)) => match result {
                Ok(call) => call.into_inner(),
                Err(status) => {
                    warn!("Forward call failed: {status}");
                    return;
                }
            },
            _ = self.cancel.cancelled() => return,
        };

        // The call is opened before the announcement is read, so that a
        // malformed one is answered with a `cancel`, which leaves the bundle
        // queued, rather than left to outlast the server's claim bound, which
        // would close the session.
        let id = BundleId::from_key(&bundle_id);
        let cla_addr = address
            .ok_or(AddressError::Unspecified)
            .and_then(cla::ClaAddress::try_from);
        let (id, cla_addr) = match (id, cla_addr) {
            (Ok(id), Ok(cla_addr)) => (id, cla_addr),
            (Err(_), _) => {
                warn!("cancelling forwarding {bundle_id}: malformed bundle id");
                let _ = requests_tx.send(ForwardRequest::cancel()).await;
                return;
            }
            (_, Err(error)) => {
                warn!("cancelling forwarding {bundle_id}: {error}");
                let _ = requests_tx.send(ForwardRequest::cancel()).await;
                return;
            }
        };

        let mut receiver = ChunkReceiver::new(&mut responses, &self.cancel);
        let result = self
            .cla
            .forward(lane, &cla_addr, &id, bundle_size, &mut receiver)
            .await;

        match result {
            Ok(result) => {
                let result = match result {
                    ForwardBundleResult::Sent => forward_result::Result::Sent(()),
                    ForwardBundleResult::NoNeighbour => forward_result::Result::NoNeighbour(()),
                    ForwardBundleResult::Accepted => forward_result::Result::Accepted(()),
                };
                if requests_tx
                    .send(ForwardRequest {
                        request: Some(forward_request::Request::Result(ForwardResult {
                            result: Some(result),
                        })),
                    })
                    .await
                    .is_ok()
                {
                    tokio::select! {
                        biased;
                        _ = async { while responses.message().await.is_ok_and(|message| message.is_some()) {} } => {}
                        _ = self.cancel.cancelled() => {}
                    }
                }
            }
            Err(error) => {
                warn!("failed to forward {bundle_id}: {error}");
                let _ = requests_tx.send(ForwardRequest::cancel()).await;
            }
        }
    }
}

/// A registered CLA's session, from the registration handshake to the end of
/// its event stream.
pub struct ClaSession {
    ctx: Arc<ClaSessionContext>,
    requests_tx: WeakSender<SubscribeRequest>,
    events: Streaming<SubscribeResponse>,
}

impl ClaSession {
    /// Opens the session and registers the CLA.
    ///
    /// Sends `Register` with `name` and the declarations in `init`, waits for the
    /// `Registration` event, and runs the CLA's `on_register` with its sink, the
    /// BPA's node ids, and the bundle size limit the BPA agreed to, before
    /// returning the node ids and the session, which
    /// [`handle_events`](ClaSession::handle_events) then drives. `cancel` is the
    /// token that ends the session from this side.
    ///
    /// # Errors
    ///
    /// Returns `Internal` carrying a [`LaneCountError`] if `init` declares more
    /// than [`MAX_LANE_COUNT`] lanes, before anything is sent; otherwise the
    /// BPA's error as recorded on the status, `AlreadyExists` for a bare `ALREADY_EXISTS`, `Disconnected` if the
    /// BPA is unreachable or `cancel` fires during the handshake, and `Internal`
    /// for any other status or for a node id that does not parse.
    pub async fn subscribe(
        channel: Channel,
        name: String,
        cla: Arc<dyn Cla>,
        init: ClaInit,
        cancel: CancellationToken,
    ) -> Result<(Vec<NodeId>, Self)> {
        let mut client = ClaServiceClient::new(channel)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE);

        let lane_count = init.lane_count.map(NonZeroU32::get);
        if let Some(declared) = lane_count
            && declared > MAX_LANE_COUNT
        {
            return Err(Error::Internal(Box::new(LaneCountError {
                declared,
                max: MAX_LANE_COUNT,
            })));
        }

        let (requests_tx, requests_rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
        let weak_requests_tx = requests_tx.downgrade();
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: name.clone(),
                    address_type: init
                        .address_type
                        .map(|address_type| ClaAddressType::from(address_type) as i32),
                    lane_count,
                    max_bundle_size: init.max_bundle_size.map(NonZeroU64::get),
                    // The SDK buffers at `DEFAULT_CHUNK_SIZE`, so it asks for no smaller ceiling.
                    max_chunk_size: None,
                })),
            })
            .await
            .map_err(|error| Error::Internal(error.into()))?;

        let (events, registration) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Disconnected),
            handshake = async {
                let mut events = client
                    .subscribe(ReceiverStream::new(requests_rx))
                    .await
                    .map_err(|status| registration_error(status, &name))?
                    .into_inner();
                let registration = expect_registration(&mut events)
                    .await
                    .map_err(|status| registration_error(status, &name))?;
                Ok::<_, Error>((events, registration))
            } => handshake?,
        };
        let node_ids = registration
            .node_ids
            .iter()
            .map(|node_id| node_id.parse::<NodeId>())
            // Qualified: `Result` here is the CLA's own alias.
            .collect::<core::result::Result<Vec<_>, _>>()
            .map_err(|error| Error::Internal(error.into()))?;
        let max_bundle_size = registration.max_bundle_size.and_then(NonZeroU64::new);
        // A server announcing a size no session may run at is not one this
        // crate wrote, so the SDK runs at its own default instead.
        let max_chunk_size = registration
            .sizes
            .as_ref()
            .and_then(|sizes| MaxChunkSize::new(sizes.max_chunk_size))
            .unwrap_or_default();
        let token = Token::from(registration.session_token);

        cla.on_register(
            Box::new(GrpcClaSink {
                client: client.clone(),
                token: token.clone(),
                max_chunk_size,
                requests_tx,
            }),
            &node_ids,
            max_bundle_size,
        )
        .await;
        Ok((
            node_ids,
            Self {
                ctx: ClaSessionContext::new(cla, client, token, cancel),
                requests_tx: weak_requests_tx,
                events,
            },
        ))
    }

    /// Runs the session until its stream ends, fails, or is cancelled.
    ///
    /// Each `Forwarding` event is executed on its own task, without a concurrency
    /// bound, because the BPA already paces a CLA by the lane count it declared.
    /// On the way out forwardings still in flight are cancelled and awaited,
    /// and the CLA's `on_unregister` runs, and the session sends `Unregister`, so
    /// the BPA retires the token even when it was this side that ended the session.
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
        // The forwardings are unbounded on purpose: the server already bounds
        // how many it has in flight per session, and the BPA runs at most one
        // transfer per lane, so a bound here would only let one lane
        // head-of-line block another, which the CLA contract forbids. Beyond
        // the connection's stream limit the calls queue in HTTP/2 rather than
        // fail.
        let forwardings = TaskPool::new();
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
                subscribe_response::Event::Forwarding(forwarding) => {
                    let ctx = ctx.clone();
                    hardy_async::spawn!(forwardings, "cla_forward", async move {
                        ctx.forward(forwarding).await
                    });
                }
            }
        };
        ctx.cancel.cancel();
        forwardings.shutdown().await;
        ctx.cla.on_unregister().await;

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
