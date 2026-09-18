// The routing surface: one Subscribe session per registration, and a
// sink whose calls are the wire's token-gated route RPCs. Routing
// agents are push-only, so there are no events to translate: the
// session anchors liveness, and `handle_events` waits for the stream to
// end.

use core::ops::ControlFlow;
use std::sync::Arc;

use hardy_async::CancellationToken;
use hardy_bpa::{
    async_trait,
    routing::{Error, Result, RouteAction, RoutingAgent, RoutingSink},
};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Status, Streaming, transport::Channel};
use tracing::warn;

use super::{LocalUnregister, next_event};
use crate::{
    MAX_MESSAGE_SIZE,
    client::SUBSCRIBE_REQUEST_CAPACITY,
    routing::{
        AddRouteRequest, Register, RemoveRouteRequest, RouteActionError, SubscribeRequest,
        SubscribeResponse, Unregister, routing_agent_service_client::RoutingAgentServiceClient,
        subscribe_request, subscribe_response,
    },
    status::recover_routing_error,
    token::Token,
};

// A status carrying the wire's typed-error discriminator recovers as
// the exact domain error the server raised; otherwise the status code
// classifies it.
fn routing_error(status: Status) -> Error {
    if let Some(e) = recover_routing_error(&status) {
        return e;
    }
    match status.code() {
        Code::Unauthenticated | Code::Unavailable => Error::Disconnected,
        Code::AlreadyExists => Error::AlreadyExists(status.message().to_string()),
        _ => Error::Internal(status.into()),
    }
}

// The session-ending counterpart of `routing_error`: any unclassified
// ending is carried whole, so the registration handle shows the actual
// failure.
fn routing_session_error(status: Status) -> Error {
    recover_routing_error(&status).unwrap_or_else(|| Error::Internal(status.into()))
}

// Dropping the sink half-closes the session stream, which the BPA
// treats as an Unregister, withdrawing the agent's routes, so `Drop`
// records the ending as this side's.
pub struct GrpcRoutingSink {
    client: RoutingAgentServiceClient<Channel>,
    token: Token,
    requests_tx: Sender<SubscribeRequest>,
    local_unregister: Arc<LocalUnregister>,
}

impl Drop for GrpcRoutingSink {
    fn drop(&mut self) {
        self.local_unregister.record();
    }
}

#[async_trait]
impl RoutingSink for GrpcRoutingSink {
    async fn unregister(&self) {
        self.local_unregister.record();
        let _ = self
            .requests_tx
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
    ) -> Result<bool> {
        let response = self
            .client
            .clone()
            .add_route(AddRouteRequest {
                session_token: self.token.to_bytes(),
                pattern: pattern.to_string(),
                action: Some(
                    (&action)
                        .try_into()
                        .map_err(|e: RouteActionError| Error::Internal(e.into()))?,
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
    ) -> Result<bool> {
        let response = self
            .client
            .clone()
            .remove_route(RemoveRouteRequest {
                session_token: self.token.to_bytes(),
                pattern: pattern.to_string(),
                // Fallible for the same reserved-reason refusal as
                // `add_route`.
                action: Some(
                    action
                        .try_into()
                        .map_err(|e: RouteActionError| Error::Internal(e.into()))?,
                ),
                priority,
            })
            .await
            .map_err(routing_error)?
            .into_inner();
        Ok(response.removed)
    }
}

/// A live session, ready to run: the event stream it anchors and the
/// agent it closes over. Obtained from
/// [`subscribe`](RoutingSession::subscribe), consumed by
/// [`handle_events`](RoutingSession::handle_events).
pub struct RoutingSession {
    agent: Arc<dyn RoutingAgent>,
    // Fires on the client's shutdown; the anchor loop races it.
    cancel: CancellationToken,
    // Set by the sink; read once the stream ends, to tell an unregister
    // of ours from the BPA dropping the registration.
    local_unregister: Arc<LocalUnregister>,
    events: Streaming<SubscribeResponse>,
}

impl RoutingSession {
    /// The Subscribe handshake plus component registration: opens the
    /// session on `channel`, hands the sink to the agent via
    /// `on_register`, and returns the BPA's node ids with the runnable
    /// session, which therefore cannot exist without having
    /// registered.
    /// `cancel` is the client's token.
    pub async fn subscribe(
        channel: Channel,
        name: String,
        agent: Arc<dyn RoutingAgent>,
        cancel: CancellationToken,
    ) -> Result<(Vec<NodeId>, Self)> {
        let mut client = RoutingAgentServiceClient::new(channel)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE);

        // The wire requires Register first, sent without waiting for
        // response headers.
        let (requests_tx, requests_rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register { name })),
            })
            .await
            .map_err(|e| Error::Internal(e.into()))?;

        let mut events = client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .map_err(routing_error)?
            .into_inner();

        let Some(SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(registration)),
        }) = events.message().await.map_err(routing_error)?
        else {
            return Err(Error::Internal(
                "The first event must be Registration".into(),
            ));
        };
        let node_ids = registration
            .node_ids
            .iter()
            .map(|s| s.parse::<NodeId>())
            // Spelled out: the crate's `Result` here is the routing
            // one, which takes a single parameter.
            .collect::<core::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Internal(e.into()))?;

        let local_unregister = Arc::new(LocalUnregister::default());
        agent
            .on_register(
                Box::new(GrpcRoutingSink {
                    client,
                    token: Token::from(registration.session_token),
                    requests_tx,
                    local_unregister: local_unregister.clone(),
                }),
                &node_ids,
            )
            .await;
        Ok((
            node_ids,
            Self {
                agent,
                cancel,
                local_unregister,
                events,
            },
        ))
    }

    /// The session anchor: routing agents receive no events, so the loop
    /// only waits for the session to end; anything on the stream is a
    /// contract violation by the server, logged. Returns `Ok(())` when
    /// the session ends at this side's asking (the client's shutdown, or
    /// an unregister of ours round-tripping), and `Err` when it ends any
    /// other way: [`Disconnected`](routing::Error::Disconnected) for a
    /// BPA that closed a session nobody here asked to end, and the
    /// stream's own error for a failure. The agent's
    /// `on_unregister` runs here: `handle_events` closes the lifecycle
    /// [`subscribe`](Self::subscribe) opened.
    pub async fn handle_events(self) -> Result<()> {
        let Self {
            agent,
            cancel,
            local_unregister,
            mut events,
        } = self;
        let result = loop {
            match next_event(&mut events, &cancel).await {
                ControlFlow::Continue(SubscribeResponse { event }) => {
                    if event.is_some() {
                        // Not Debug-formatted: a Registration carries
                        // the session token, which must never reach the
                        // logs.
                        warn!("Ignoring unexpected event on the session stream");
                    }
                }
                ControlFlow::Break(None) if local_unregister.solicited(&cancel) => break Ok(()),
                ControlFlow::Break(None) => break Err(Error::Disconnected),
                ControlFlow::Break(Some(status)) => break Err(routing_session_error(status)),
            }
        };
        agent.on_unregister().await;
        result
    }
}
