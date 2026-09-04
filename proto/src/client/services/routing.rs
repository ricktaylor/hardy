// The routing surface of the client SDK: one Subscribe session per
// registration, and a sink whose calls are the wire's token-gated route
// RPCs. Routing agents are push-only, so there are no events to
// translate: the session's down direction carries the Registration and
// then anchors liveness, and `run_session` just waits for the stream to
// end. Declarations are ordered define-before-reference: the wire
// conversions, the sink, the event loop, then the handshake.

use core::ops::ControlFlow;

use hardy_async::CancellationToken;
use hardy_bpa::{
    Bytes, async_trait,
    routing::{self, RouteAction, RoutingSink},
};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Status, Streaming, transport::Channel};
use tracing::warn;

use super::super::SUBSCRIBE_REQUEST_CAPACITY;
use super::next_event;
use crate::{
    MAX_MESSAGE_SIZE,
    error_status::recover_routing_error,
    routing::{
        AddRouteRequest, Register, RemoveRouteRequest, RouteActionError, SubscribeRequest,
        SubscribeResponse, Unregister, routing_agent_service_client::RoutingAgentServiceClient,
        subscribe_request, subscribe_response,
    },
};

// Wire statuses become routing errors. A status carrying the wire's
// typed-error discriminator recovers as the exact domain error the
// server raised; otherwise (a non-Hardy server, or a kind whose payload
// cannot travel) the status code classifies it: a dead token or an
// unreachable BPA is the sink's disconnection, a duplicate name is the
// local registry's error, everything else carries through.
fn routing_error(status: Status) -> routing::Error {
    if let Some(e) = recover_routing_error(&status) {
        return e;
    }
    match status.code() {
        Code::Unauthenticated | Code::Unavailable => routing::Error::Disconnected,
        Code::AlreadyExists => routing::Error::AlreadyExists(status.message().to_string()),
        _ => routing::Error::Internal(status.into()),
    }
}

// The session-ending counterpart of `routing_error`: a server-classified
// status recovers as its exact domain error, any other ending is
// carried whole so awaiting the registration handle shows the actual
// failure (see `session_error`).
fn routing_session_error(status: Status) -> routing::Error {
    recover_routing_error(&status).unwrap_or_else(|| routing::Error::Internal(status.into()))
}

// The registration's sink: every call is a token-gated RPC of the
// session. No Drop impl is needed: dropping the sink drops the request
// sender, which half-closes the session stream, and the BPA treats that
// exactly as an Unregister (withdrawing the agent's routes).
pub struct GrpcRoutingSink {
    client: RoutingAgentServiceClient<Channel>,
    token: Bytes,
    requests: Sender<SubscribeRequest>,
}

#[async_trait]
impl RoutingSink for GrpcRoutingSink {
    async fn unregister(&self) {
        let _ = self
            .requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await;
    }

    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> routing::Result<bool> {
        let response = self
            .client
            .clone()
            .add_route(AddRouteRequest {
                session_token: self.token.clone(),
                pattern: pattern.to_string(),
                action: Some(
                    (&action)
                        .try_into()
                        .map_err(|e: RouteActionError| routing::Error::Internal(e.into()))?,
                ),
                priority,
            })
            .await
            .map_err(routing_error)?
            .into_inner();
        Ok(response.added)
    }

    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &RouteAction,
        priority: u32,
    ) -> routing::Result<bool> {
        let response = self
            .client
            .clone()
            .remove_route(RemoveRouteRequest {
                session_token: self.token.clone(),
                pattern: pattern.to_string(),
                // Fallible for the same reserved-reason refusal as
                // `add_route`.
                action: Some(
                    action
                        .try_into()
                        .map_err(|e: RouteActionError| routing::Error::Internal(e.into()))?,
                ),
                priority,
            })
            .await
            .map_err(routing_error)?
            .into_inner();
        Ok(response.removed)
    }
}

// The session anchor: routing agents receive no events, so the loop only
// waits for the session to end. An event on this stream is a contract
// violation by the server and is logged. Returns `Ok(())` when the
// session ends cleanly (the client's shutdown or a server half-close)
// and `Err` when the stream fails; the caller owns the agent's
// `on_unregister`.
pub async fn run_session(
    mut events: Streaming<SubscribeResponse>,
    cancel: CancellationToken,
) -> routing::Result<()> {
    // Routing agents receive no events: the session's down direction only
    // anchors liveness, so anything on it is a contract violation, logged.
    loop {
        match next_event(&mut events, &cancel).await {
            ControlFlow::Continue(SubscribeResponse { event }) => {
                if let Some(event) = event {
                    warn!("Ignoring unexpected routing event: {event:?}");
                }
            }
            ControlFlow::Break(None) => return Ok(()),
            ControlFlow::Break(Some(status)) => return Err(routing_session_error(status)),
        }
    }
}

// The Subscribe handshake: Register up, Registration down, and the
// session is live. Returns the BPA's node ids, the sink, and the event
// stream, which run_session anchors from then on.
pub async fn subscribe(
    channel: Channel,
    name: String,
) -> routing::Result<(Vec<NodeId>, GrpcRoutingSink, Streaming<SubscribeResponse>)> {
    let mut client = RoutingAgentServiceClient::new(channel)
        .max_encoding_message_size(MAX_MESSAGE_SIZE)
        .max_decoding_message_size(MAX_MESSAGE_SIZE);

    // The wire requires Register first, sent without waiting for
    // response headers.
    let (requests, rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
    requests
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register { name })),
        })
        .await
        .map_err(|e| routing::Error::Internal(e.into()))?;

    let mut events = client
        .subscribe(ReceiverStream::new(rx))
        .await
        .map_err(routing_error)?
        .into_inner();

    let Some(SubscribeResponse {
        event: Some(subscribe_response::Event::Registration(registration)),
    }) = events.message().await.map_err(routing_error)?
    else {
        return Err(routing::Error::Internal(
            "The first event must be Registration".into(),
        ));
    };
    let node_ids = registration
        .node_ids
        .iter()
        .map(|s| s.parse::<NodeId>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| routing::Error::Internal(e.into()))?;

    Ok((
        node_ids,
        GrpcRoutingSink {
            client,
            token: registration.session_token,
            requests,
        },
        events,
    ))
}
