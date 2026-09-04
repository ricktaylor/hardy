// The convergence-layer surface of the client SDK: one Subscribe
// session per registration, a sink whose calls are the wire's
// token-gated RPCs, and an event loop that turns each announced
// `Forwarding` into a `Cla::forward` call. Declarations are
// ordered define-before-reference: the wire conversions, the sink, the
// forwarding runner, the event loop, then the handshake.

use core::{num::NonZeroU32, ops::ControlFlow};
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::{
    Bytes, async_trait,
    cla::{self, Cla, ForwardBundleResult, Sink, TransferOutcome},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{bundle, eid::NodeId};
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Status, Streaming, transport::Channel};
use tracing::warn;

use super::super::{SUBSCRIBE_REQUEST_CAPACITY, TRANSFER_REQUEST_CAPACITY, adapter};
use super::next_event;
use crate::{
    MAX_MESSAGE_SIZE,
    cla::{
        AddPeerRequest, ClaAddress, ClaAddressType, DispatchMetadata, DispatchRequest,
        ForwardMetadata, ForwardRequest, ForwardResult, Forwarding, Register, RemovePeerRequest,
        ReportTransferOutcomeRequest, SubscribeRequest, SubscribeResponse, Unregister,
        cla_service_client::ClaServiceClient, dispatch_request, forward_request, forward_result,
        report_transfer_outcome_request, subscribe_request, subscribe_response,
    },
    error_status::recover_cla_error,
    stream::Cancel,
};

// Wire statuses become cla errors. A status carrying the wire's
// typed-error discriminator recovers as the exact domain error the
// server raised; otherwise (a non-Hardy server, or a kind whose payload
// cannot travel) the status code classifies it: a dead token or an
// unreachable BPA is the sink's disconnection, a duplicate name is the
// local registry's error, a cancelled stream carries through,
// everything else is internal.
fn cla_error(status: Status) -> cla::Error {
    if let Some(e) = recover_cla_error(&status) {
        return e;
    }
    match status.code() {
        Code::Unauthenticated | Code::Unavailable => cla::Error::Disconnected,
        Code::AlreadyExists => cla::Error::AlreadyExists(status.message().to_string()),
        Code::Cancelled => cla::Error::StreamCancelled,
        _ => cla::Error::Internal(status.into()),
    }
}

// The session-ending counterpart of `cla_error`: a server-classified
// status recovers as its exact domain error, any other ending is
// carried whole so awaiting the registration handle shows the actual
// failure (see `session_error`).
fn cla_session_error(status: Status) -> cla::Error {
    recover_cla_error(&status).unwrap_or_else(|| cla::Error::Internal(status.into()))
}

// The registration's sink: every call is a token-gated RPC of the
// session. No Drop impl is needed: dropping the sink drops the request
// sender, which half-closes the session stream, and the BPA treats that
// exactly as an Unregister.
pub struct GrpcClaSink {
    client: ClaServiceClient<Channel>,
    token: Bytes,
    requests: Sender<SubscribeRequest>,
}

#[async_trait]
impl Sink for GrpcClaSink {
    async fn unregister(&self) {
        let _ = self
            .requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await;
    }

    async fn dispatch(
        &self,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&cla::ClaAddress>,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<()> {
        // tonic wants a `'static` request stream, and `stream` is
        // borrowed for this call: the pump bridges them, sending the
        // metadata then pulling segments and pushing wire chunks while
        // the call runs, both driven by the same join.
        let metadata = DispatchMetadata {
            session_token: self.token.clone(),
            peer_node_id: peer_node.map(ToString::to_string),
            peer_addr: peer_addr.cloned().map(ClaAddress::from),
        };
        let (requests, rx) = mpsc::channel::<DispatchRequest>(TRANSFER_REQUEST_CAPACITY);
        let pump = async move {
            if requests
                .send(DispatchRequest {
                    request: Some(dispatch_request::Request::Metadata(metadata)),
                })
                .await
                .is_err()
            {
                return;
            }
            adapter::Writer::new(&requests).write_all(stream).await;
        };

        let mut client = self.client.clone();
        let ((), response) = tokio::join!(pump, client.dispatch(ReceiverStream::new(rx)));
        // `cla_error` maps a cancelled stream to `StreamCancelled`, the
        // same error a local streamed dispatch returns when its producer
        // gives up before the final segment.
        response.map_err(cla_error)?;
        Ok(())
    }

    async fn add_peer(&self, cla_addr: cla::ClaAddress, node_ids: &[NodeId]) -> cla::Result<bool> {
        let response = self
            .client
            .clone()
            .add_peer(AddPeerRequest {
                session_token: self.token.clone(),
                node_ids: node_ids.iter().map(ToString::to_string).collect(),
                address: Some(cla_addr.into()),
            })
            .await
            .map_err(cla_error)?
            .into_inner();
        Ok(response.added)
    }

    async fn remove_peer(&self, cla_addr: &cla::ClaAddress) -> cla::Result<bool> {
        let response = self
            .client
            .clone()
            .remove_peer(RemovePeerRequest {
                session_token: self.token.clone(),
                address: Some(cla_addr.clone().into()),
            })
            .await
            .map_err(cla_error)?
            .into_inner();
        Ok(response.removed)
    }

    async fn transfer_outcome(
        &self,
        bundle_id: &bundle::Id,
        outcome: TransferOutcome,
    ) -> cla::Result<()> {
        let outcome = match outcome {
            TransferOutcome::Completed => report_transfer_outcome_request::Outcome::Completed(()),
            TransferOutcome::Failed => report_transfer_outcome_request::Outcome::Failed(()),
        };
        self.client
            .clone()
            .report_transfer_outcome(ReportTransferOutcomeRequest {
                session_token: self.token.clone(),
                bundle_id: bundle_id.to_key(),
                outcome: Some(outcome),
            })
            .await
            .map_err(cla_error)?;
        Ok(())
    }
}

// Runs one announced forwarding to its terminal result: opens the
// Forward call, streams the bundle down into the CLA through the
// shared receiver, and sends the result back up. The receiver's
// in-band cancel on drop is the abandonment signal for a forwarding
// the CLA declines without a terminal result, and it is a no-op once
// the server has completed the call on the result. Every await that
// can outlast the session races `cancel`, so a stuck server or CLA
// cannot hang the client's shutdown.
async fn run_forwarding(
    cla: Arc<dyn Cla>,
    mut client: ClaServiceClient<Channel>,
    token: Bytes,
    forwarding: Forwarding,
    cancel: CancellationToken,
) {
    let Forwarding {
        bundle_id,
        address,
        lane,
        bundle_size,
    } = forwarding;

    // Open the Forward call first: the server parks the bundle awaiting
    // it, so the call must be made even for an announcement we cannot
    // act on, or the bundle would strand until the session ends. The
    // wire requires the metadata first, sent without waiting for
    // response headers; the bundle bytes then stream down as the
    // response.
    let (requests, rx) = mpsc::channel::<ForwardRequest>(TRANSFER_REQUEST_CAPACITY);
    if requests
        .send(ForwardRequest {
            request: Some(forward_request::Request::Metadata(ForwardMetadata {
                session_token: token,
                bundle_id: bundle_id.clone(),
            })),
        })
        .await
        .is_err()
    {
        return;
    }
    let response = tokio::select! {
        biased;
        response = client.forward(ReceiverStream::new(rx)) => response,
        // Teardown while the call is opening: dropping the request
        // sender abandons it, and the BPA requeues the bundle.
        _ = cancel.cancelled() => return,
    };
    let chunks = match response {
        Ok(response) => response.into_inner(),
        Err(status) => {
            warn!("Forward call rejected: {status}");
            return;
        }
    };

    // A malformed announcement (a bad address or bundle id, which the
    // server should never send) is declined in-band, so the BPA requeues
    // the bundle promptly rather than holding it until expiry.
    // A declined address uses the shared `TryFrom` with `.ok()`: the
    // client has no caller to report a `Status` to, it just declines.
    let (Ok(id), Some(cla_addr)) = (
        bundle::Id::from_key(&bundle_id),
        address.and_then(|a| cla::ClaAddress::try_from(a).ok()),
    ) else {
        warn!("Declining an unusable forwarding for bundle {bundle_id}");
        let _ = requests.send(ForwardRequest::cancel()).await;
        return;
    };

    let mut receiver = adapter::Reader::new(chunks, requests.clone());
    let forward = cla.forward(lane, &cla_addr, &id, bundle_size, &mut receiver);
    // Completion is polled first so a forward that finished in the same
    // instant the session ended is honoured; a pending one yields to the
    // teardown immediately.
    let result = tokio::select! {
        biased;
        result = forward => result,
        // Teardown mid-transfer: dropping the receiver abandons the
        // forwarding with the wire's in-band cancel, and the BPA
        // requeues the bundle.
        _ = cancel.cancelled() => return,
    };

    match result {
        Ok(result) => {
            let result = match result {
                ForwardBundleResult::Sent => forward_result::Result::Sent(()),
                ForwardBundleResult::NoNeighbour => forward_result::Result::NoNeighbour(()),
                ForwardBundleResult::Accepted => forward_result::Result::Accepted(()),
            };
            let sent = requests
                .send(ForwardRequest {
                    request: Some(forward_request::Request::Result(ForwardResult {
                        result: Some(result),
                    })),
                })
                .await;

            // The server completes the call on the result, so the response
            // is drained to its end before the streams drop: tearing the
            // response half down early resets the stream, which can
            // discard the queued result and fail a transfer the CLA
            // answered. The drain races the teardown so a server that
            // never ends the call cannot pin the session's shutdown.
            if sent.is_ok() {
                let drain = async { while receiver.recv().await.is_ok() {} };
                tokio::select! {
                    biased;
                    _ = drain => {}
                    _ = cancel.cancelled() => {}
                }
            }
        }
        // The wire's ForwardResult has no failed variant, so the CLA's
        // error cannot travel as a terminal result: it is logged here,
        // and dropping the receiver abandons the call with the in-band
        // cancel, on which the BPA requeues the bundle.
        Err(e) => warn!("Forwarding {bundle_id} failed: {e}"),
    }
}

// The session's event loop: each announced forwarding is executed on its
// own task so a slow transfer never stalls the next announcement. The
// pool is deliberately unbounded, unlike the delivery surfaces:
// forwarding concurrency is BPA-driven (the BPA holds one `Cla::forward`
// call open per egress queue and announces a queue's next bundle only
// when the current one resolves), so a well-behaved BPA cannot flood
// this pool, and a client-side bound would couple unrelated peers: a
// stalled transfer to one peer would hold a slot for minutes and starve
// announcements bound for healthy peers, while also stopping this loop
// from observing the stream's end. Returns `Ok(())` when the session
// ends cleanly (the client's shutdown or a server half-close) and `Err`
// when the stream fails; the caller owns the CLA's `on_unregister`.
pub async fn run_session(
    mut events: Streaming<SubscribeResponse>,
    cla: Arc<dyn Cla>,
    cancel: CancellationToken,
    client: ClaServiceClient<Channel>,
    token: Bytes,
) -> cla::Result<()> {
    // Every in-flight forwarding races `session_cancel`, which fires on
    // the client's shutdown (it is a child of `cancel`) and at this
    // session's own end.
    let session_cancel = cancel.child_token();
    let forwardings = TaskPool::new();
    let result = loop {
        let SubscribeResponse { event } = match next_event(&mut events, &cancel).await {
            ControlFlow::Continue(response) => response,
            ControlFlow::Break(None) => break Ok(()),
            ControlFlow::Break(Some(status)) => break Err(cla_session_error(status)),
        };
        let Some(event) = event else {
            warn!("Ignoring event with no payload");
            continue;
        };
        match event {
            subscribe_response::Event::Registration(registration) => {
                warn!("Ignoring unexpected event: {registration:?}")
            }
            subscribe_response::Event::Forwarding(forwarding) => {
                let cla = cla.clone();
                let client = client.clone();
                let token = token.clone();
                let cancel = session_cancel.clone();
                hardy_async::spawn!(forwardings, "cla_forward", async move {
                    run_forwarding(cla, client, token, forwarding, cancel).await
                });
            }
        }
    };
    // In-flight forwardings end before the session is declared over, so no
    // `forward` call outlives the caller's `on_unregister`.
    session_cancel.cancel();
    forwardings.shutdown().await;
    result
}

// A completed CLA registration: the BPA's node ids, the sink for the
// CLA, the event stream, and the client and token `run_session` opens
// Forward calls with.
pub struct Registered {
    pub node_ids: Vec<NodeId>,
    pub sink: GrpcClaSink,
    pub events: Streaming<SubscribeResponse>,
    pub client: ClaServiceClient<Channel>,
    pub token: Bytes,
}

// The Subscribe handshake: Register up, Registration down, and the
// session is live.
pub async fn subscribe(
    channel: Channel,
    name: String,
    address_type: Option<cla::ClaAddressType>,
    lane_count: Option<NonZeroU32>,
) -> cla::Result<Registered> {
    let mut client = ClaServiceClient::new(channel)
        .max_encoding_message_size(MAX_MESSAGE_SIZE)
        .max_decoding_message_size(MAX_MESSAGE_SIZE);

    // An over-declared lane count is an error here, not a silent
    // clamp: the BPA rejects it, and a clamped registration would
    // leave the CLA believing in lanes the BPA never offers.
    let lane_count = lane_count
        .map(|n| {
            (n.get() <= cla::MAX_LANE_COUNT)
                .then_some(n.get())
                .ok_or_else(|| {
                    cla::Error::Internal(
                        format!(
                            "lane_count {n} exceeds the maximum of {}",
                            cla::MAX_LANE_COUNT
                        )
                        .into(),
                    )
                })
        })
        .transpose()?;

    // The wire requires Register first, sent without waiting for
    // response headers.
    let (requests, rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
    requests
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                name,
                address_type: address_type.map(|t| ClaAddressType::from(t) as i32),
                lane_count,
            })),
        })
        .await
        .map_err(|e| cla::Error::Internal(e.into()))?;

    let mut events = client
        .subscribe(ReceiverStream::new(rx))
        .await
        .map_err(cla_error)?
        .into_inner();

    let Some(SubscribeResponse {
        event: Some(subscribe_response::Event::Registration(registration)),
    }) = events.message().await.map_err(cla_error)?
    else {
        return Err(cla::Error::Internal(
            "The first event must be Registration".into(),
        ));
    };
    let node_ids = registration
        .node_ids
        .iter()
        .map(|s| s.parse::<NodeId>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| cla::Error::Internal(e.into()))?;

    let token = registration.session_token;
    Ok(Registered {
        node_ids,
        sink: GrpcClaSink {
            client: client.clone(),
            token: token.clone(),
            requests,
        },
        events,
        client,
        token,
    })
}
