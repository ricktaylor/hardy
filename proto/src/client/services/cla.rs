// Client of the convergence-layer wire surface: one Subscribe session
// per registration.

use core::{num::NonZeroU64, ops::ControlFlow};
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::{
    async_trait,
    cla::{self, Cla, ClaInit, Error, ForwardBundleResult, Result, Sink, TransferOutcome},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{bundle::Id as BundleId, eid::NodeId};
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Status, Streaming, transport::Channel};
use tracing::warn;

use super::{LocalUnregister, next_event};
use crate::{
    MAX_LANE_COUNT, MAX_MESSAGE_SIZE,
    cla::{
        Acceptance, AddPeerRequest, ClaAddress, ClaAddressType, DispatchMetadata, DispatchRequest,
        ForwardMetadata, ForwardRequest, ForwardResult, Forwarding, Register, RemovePeerRequest,
        ReportTransferOutcomeRequest, SubscribeRequest, SubscribeResponse, Unregister,
        cla_service_client::ClaServiceClient, dispatch_request, forward_request, forward_result,
        report_transfer_outcome_request, subscribe_request, subscribe_response,
    },
    client::{
        SUBSCRIBE_REQUEST_CAPACITY, TRANSFER_REQUEST_CAPACITY, adapter::ResponseReader,
        write_transfer,
    },
    grammar::Cancel,
    status::recover_cla_error,
    token::Token,
};

// Maps a failed call to a domain error.
fn cla_error(status: Status) -> Error {
    if let Some(e) = recover_cla_error(&status) {
        return e;
    }
    match status.code() {
        Code::Unauthenticated | Code::Unavailable => Error::Disconnected,
        Code::AlreadyExists => Error::AlreadyExists(status.message().to_string()),
        Code::Cancelled => Error::StreamCancelled,
        _ => Error::Internal(status.into()),
    }
}

// Maps a session-ending status. Unlike `cla_error`, there is no
// fallback classification by status code: anything without a
// discriminator becomes `Internal`.
fn cla_session_error(status: Status) -> Error {
    recover_cla_error(&status).unwrap_or_else(|| Error::Internal(status.into()))
}

// Dropping the sink drops `requests_tx`, half-closing the session
// stream; the BPA treats that as an Unregister, so `Drop` records the
// ending as locally requested.
pub struct GrpcClaSink {
    client: ClaServiceClient<Channel>,
    token: Token,
    requests_tx: Sender<SubscribeRequest>,
    local_unregister: Arc<LocalUnregister>,
}

impl Drop for GrpcClaSink {
    fn drop(&mut self) {
        self.local_unregister.record();
    }
}

#[async_trait]
impl Sink for GrpcClaSink {
    async fn unregister(&self) {
        self.local_unregister.record();
        let _ = self
            .requests_tx
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
    ) -> Result<()> {
        let metadata = DispatchRequest {
            request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                session_token: self.token.to_bytes(),
                peer_node_id: peer_node.map(ToString::to_string),
                peer_addr: peer_addr.cloned().map(ClaAddress::from),
            })),
        };
        let mut client = self.client.clone();
        let response = write_transfer(metadata, stream, |requests| client.dispatch(requests)).await;

        // Anything but an explicit acceptance is a refusal.
        // `StreamCancelled` stands in for a refusal until
        // `cla::Acceptance` lands (see `proto/docs/TODO.md`).
        match response.map_err(cla_error)?.into_inner().acceptance() {
            Acceptance::Accepted => Ok(()),
            Acceptance::Refused | Acceptance::Unspecified => Err(Error::StreamCancelled),
        }
    }

    async fn add_peer(&self, cla_addr: cla::ClaAddress, node_ids: &[NodeId]) -> Result<bool> {
        let response = self
            .client
            .clone()
            .add_peer(AddPeerRequest {
                session_token: self.token.to_bytes(),
                node_ids: node_ids.iter().map(ToString::to_string).collect(),
                address: Some(cla_addr.into()),
            })
            .await
            .map_err(cla_error)?
            .into_inner();
        Ok(response.added)
    }
    async fn remove_peer(&self, cla_addr: &cla::ClaAddress) -> Result<bool> {
        let response = self
            .client
            .clone()
            .remove_peer(RemovePeerRequest {
                session_token: self.token.to_bytes(),
                address: Some(cla_addr.clone().into()),
            })
            .await
            .map_err(cla_error)?
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
                session_token: self.token.to_bytes(),
                bundle_id: bundle_id.to_key(),
                outcome: Some(outcome),
            })
            .await
            .map_err(cla_error)?;
        Ok(())
    }
}

// Session state shared by the event loop and its forwarding tasks.
struct ClaSessionCtx {
    cla: Arc<dyn Cla>,
    client: ClaServiceClient<Channel>,
    token: Token,
    // Child of the caller's token; cancelling it ends the event loop
    // and the forwarding tasks.
    cancel: CancellationToken,
    // Distinguishes a local unregister from the BPA ending the session.
    local_unregister: Arc<LocalUnregister>,
}

impl ClaSessionCtx {
    fn new(
        cla: Arc<dyn Cla>,
        client: ClaServiceClient<Channel>,
        token: Token,
        cancel: CancellationToken,
        local_unregister: Arc<LocalUnregister>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cla,
            client,
            token,
            cancel: cancel.child_token(),
            local_unregister,
        })
    }

    async fn forward(self: Arc<Self>, forwarding: Forwarding) {
        let Forwarding {
            bundle_id,
            address,
            lane,
            bundle_size,
        } = forwarding;
        let mut client = self.client.clone();

        // Open the call before validating the forwarding, so an
        // unusable one can be declined in band and the BPA requeues
        // the bundle promptly.
        let (requests_tx, requests_rx) = mpsc::channel::<ForwardRequest>(TRANSFER_REQUEST_CAPACITY);
        if requests_tx
            .send(ForwardRequest {
                request: Some(forward_request::Request::Metadata(ForwardMetadata {
                    session_token: self.token.to_bytes(),
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
                    warn!("Forward call rejected: {status}");
                    return;
                }
            },
            _ = self.cancel.cancelled() => return,
        };

        let Some((id, cla_addr)) = BundleId::from_key(&bundle_id)
            .ok()
            .zip(address.and_then(|a| cla::ClaAddress::try_from(a).ok()))
        else {
            warn!("Declining an unusable forwarding for bundle {bundle_id}");
            let _ = requests_tx.send(ForwardRequest::cancel()).await;
            return;
        };

        let mut reader = ResponseReader::new(&mut responses);
        // Biased: a completed forward is reported even when
        // cancellation is also pending.
        let result = tokio::select! {
            biased;
            result = self.cla.forward(lane, &cla_addr, &id, bundle_size, &mut reader) => result,
            _ = self.cancel.cancelled() => return,
        };

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
                        _ = async { while responses.message().await.is_ok_and(|m| m.is_some()) {} } => {}
                        _ = self.cancel.cancelled() => {}
                    }
                }
            }
            // `ForwardResult` has no failed variant: cancel in band
            // instead, which leaves the bundle queued.
            Err(e) => {
                warn!("Forwarding {bundle_id} failed: {e}");
                let _ = requests_tx.send(ForwardRequest::cancel()).await;
            }
        }
    }
}

pub struct ClaSession {
    ctx: Arc<ClaSessionCtx>,
    events: Streaming<SubscribeResponse>,
}

impl ClaSession {
    // Opens the session on `channel`, hands the sink and the
    // negotiated `max_bundle_size` to `cla.on_register`, and returns
    // the BPA's node ids.
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

        // A lane count above `MAX_LANE_COUNT` is rejected; clamping it
        // would leave the CLA assuming lanes the BPA never offers.
        let lane_count = init
            .lane_count
            .map(|n| {
                (n.get() <= MAX_LANE_COUNT)
                    .then_some(n.get())
                    .ok_or_else(|| {
                        Error::Internal(
                            format!("lane_count {n} exceeds the maximum of {MAX_LANE_COUNT}")
                                .into(),
                        )
                    })
            })
            .transpose()?;

        // Register is queued before the call opens: the server sends
        // no response headers until it reads Register.
        let (requests_tx, requests_rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name,
                    address_type: init.address_type.map(|t| ClaAddressType::from(t) as i32),
                    lane_count,
                    max_bundle_size: init.max_bundle_size.map(NonZeroU64::get),
                })),
            })
            .await
            .map_err(|e| Error::Internal(e.into()))?;

        let mut events = client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .map_err(cla_error)?
            .into_inner();

        let Some(SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(registration)),
        }) = events.message().await.map_err(cla_error)?
        else {
            return Err(Error::Internal(
                "The first event must be Registration".into(),
            ));
        };
        let node_ids = registration
            .node_ids
            .iter()
            .map(|s| s.parse::<NodeId>())
            // Qualified: the `Result` in scope is the one-parameter
            // `cla::Result`.
            .collect::<core::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Internal(e.into()))?;
        let max_bundle_size = registration.max_bundle_size.and_then(NonZeroU64::new);
        let token = Token::from(registration.session_token);

        let local_unregister = Arc::new(LocalUnregister::default());
        cla.on_register(
            Box::new(GrpcClaSink {
                client: client.clone(),
                token: token.clone(),
                requests_tx,
                local_unregister: local_unregister.clone(),
            }),
            &node_ids,
            max_bundle_size,
        )
        .await;
        Ok((
            node_ids,
            Self {
                ctx: ClaSessionCtx::new(cla, client, token, cancel, local_unregister),
                events,
            },
        ))
    }

    // Drains session events, running each forwarding on its own task.
    // The pool is deliberately unbounded: the BPA paces forwardings,
    // and a client-side bound would couple unrelated peers. Runs
    // `on_unregister` after the last forwarding finishes. Returns
    // `Ok(())` when this side ended the session.
    pub async fn handle_events(self) -> Result<()> {
        let Self { ctx, mut events } = self;
        let forwardings = TaskPool::new();
        let result = loop {
            let SubscribeResponse { event } = match next_event(&mut events, &ctx.cancel).await {
                ControlFlow::Continue(response) => response,
                ControlFlow::Break(None) if ctx.local_unregister.solicited(&ctx.cancel) => {
                    break Ok(());
                }
                ControlFlow::Break(None) => break Err(Error::Disconnected),
                ControlFlow::Break(Some(status)) => break Err(cla_session_error(status)),
            };
            let Some(event) = event else {
                warn!("Ignoring event with no payload");
                continue;
            };
            match event {
                // Registration carries the session token; never
                // Debug-format it.
                subscribe_response::Event::Registration(_) => {
                    warn!("Ignoring unexpected Registration event")
                }
                subscribe_response::Event::Forwarding(forwarding) => {
                    let ctx = ctx.clone();
                    hardy_async::spawn!(forwardings, "cla_forward", async move {
                        ctx.forward(forwarding).await
                    });
                }
            }
        };
        // Drain the forwarding tasks so no `forward` call outlives
        // `on_unregister`.
        ctx.cancel.cancel();
        forwardings.shutdown().await;
        ctx.cla.on_unregister().await;
        result
    }
}
