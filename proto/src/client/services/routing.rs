// Client of the routing-agent wire surface: one Subscribe session per
// registration.

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

// Maps a failed call to a domain error.
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

// Maps a session-ending status. Unlike `routing_error`, there is no
// fallback classification by status code: anything without a
// discriminator becomes `Internal`.
fn routing_session_error(status: Status) -> Error {
    recover_routing_error(&status).unwrap_or_else(|| Error::Internal(status.into()))
}

// Dropping the sink drops `requests_tx`, half-closing the session
// stream; the BPA treats that as an Unregister, so `Drop` records the
// ending as locally requested.
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

pub struct RoutingSession {
    agent: Arc<dyn RoutingAgent>,
    cancel: CancellationToken,
    local_unregister: Arc<LocalUnregister>,
    events: Streaming<SubscribeResponse>,
}

impl RoutingSession {
    // Opens the session on `channel`, hands the sink to
    // `agent.on_register`, and returns the BPA's node ids.
    pub async fn subscribe(
        channel: Channel,
        name: String,
        agent: Arc<dyn RoutingAgent>,
        cancel: CancellationToken,
    ) -> Result<(Vec<NodeId>, Self)> {
        let mut client = RoutingAgentServiceClient::new(channel)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE);

        // Register is queued before the call opens: the server sends
        // no response headers until it reads Register.
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
            // Qualified: the `Result` in scope is the one-parameter
            // `routing::Result`.
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

    // Routing agents receive no events after Registration, so this
    // only waits for the session to end, then runs `on_unregister`.
    // Returns `Ok(())` when this side ended the session.
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
                        // The event may carry the session token; never
                        // Debug-format it.
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
