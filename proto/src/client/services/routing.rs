//! The routing agent session.

use core::ops::ControlFlow;
use std::sync::Arc;

use hardy_async::CancellationToken;
use hardy_bpa::{
    async_trait,
    routing::{Error, Result, RouteAction, RoutingAgent, RoutingSink},
};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use tokio::sync::mpsc::{self, Sender, WeakSender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Status, Streaming, transport::Channel};
use tracing::warn;

use super::{expect_registration, next_event};
use crate::{
    MAX_MESSAGE_SIZE,
    client::services::SUBSCRIBE_REQUEST_CAPACITY,
    grammar::Unregister,
    routing::{
        AddRouteRequest, Register, RemoveRouteRequest, RouteActionError, SubscribeRequest,
        SubscribeResponse, routing_service_client::RoutingServiceClient, subscribe_request,
    },
    token::Token,
};

/// Maps a status from the registration handshake to the agent's error.
///
/// As [`routing_session_error`], except that `ALREADY_EXISTS` is `name` held
/// by another session. The caller passes the name because the status does not
/// carry it: the client asked for it, so it already knows which one was
/// refused.
fn routing_error(status: Status, name: &str) -> Error {
    if status.code() == Code::AlreadyExists {
        Error::AlreadyExists(name.to_string())
    } else {
        routing_session_error(status)
    }
}

/// Maps a status from the session stream or a route call to the agent's error.
///
/// The status is a gRPC code and a message, so this reads the code the
/// contract defines: a code the server only ever sends when the session is
/// over ends the session. A routing session moves no bundles, so anything else
/// is `Internal`, which keeps the status and so its message.
fn routing_session_error(status: Status) -> Error {
    match status.code() {
        Code::Unauthenticated | Code::Unavailable | Code::DeadlineExceeded => Error::Disconnected,
        _ => Error::Internal(status.into()),
    }
}

/// The [`RoutingSink`] a routing agent receives from
/// [`RoutingSession::subscribe`]: `add_route` and `remove_route` are the unary
/// calls of the same names, and `unregister` is an `Unregister` on the session
/// stream.
///
/// A route action that has no wire form, because it drops with the reserved
/// reason code 255, fails the call with `Internal` before anything is sent.
pub struct GrpcRoutingSink {
    client: RoutingServiceClient<Channel>,
    token: Token,
    requests_tx: Sender<SubscribeRequest>,
}

#[async_trait]
impl RoutingSink for GrpcRoutingSink {
    async fn unregister(&self) {
        let _ = self.requests_tx.send(SubscribeRequest::unregister()).await;
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
                session_token: self.token.clone().into(),
                pattern: pattern.to_string(),
                action: Some(
                    (&action)
                        .try_into()
                        .map_err(|e: RouteActionError| Error::Internal(e.into()))?,
                ),
                priority,
            })
            .await
            .map_err(routing_session_error)?
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
                session_token: self.token.clone().into(),
                pattern: pattern.to_string(),
                action: Some(
                    action
                        .try_into()
                        .map_err(|e: RouteActionError| Error::Internal(e.into()))?,
                ),
                priority,
            })
            .await
            .map_err(routing_session_error)?
            .into_inner();
        Ok(response.removed)
    }
}

/// A registered routing agent's session, from the registration handshake to the
/// end of its stream.
pub struct RoutingSession {
    agent: Arc<dyn RoutingAgent>,
    cancel: CancellationToken,
    requests_tx: WeakSender<SubscribeRequest>,
    events: Streaming<SubscribeResponse>,
}

impl RoutingSession {
    /// Opens the session and registers the agent.
    ///
    /// Sends `Register` with `name`, waits for the `Registration` event, and runs
    /// the agent's `on_register` with its sink and the BPA's node ids before
    /// returning the node ids and the session, which
    /// [`handle_events`](RoutingSession::handle_events) then drives. `cancel` is
    /// the token that ends the session from this side.
    ///
    /// # Errors
    ///
    /// Returns the BPA's error as recorded on the status, `AlreadyExists` for a
    /// bare `ALREADY_EXISTS`, `Disconnected` if the BPA is unreachable or `cancel`
    /// fires during the handshake, and `Internal` for any other status or for a
    /// node id that does not parse.
    pub async fn subscribe(
        channel: Channel,
        name: String,
        agent: Arc<dyn RoutingAgent>,
        cancel: CancellationToken,
    ) -> Result<(Vec<NodeId>, Self)> {
        let mut client = RoutingServiceClient::new(channel)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_MESSAGE_SIZE);

        let (requests_tx, requests_rx) = mpsc::channel(SUBSCRIBE_REQUEST_CAPACITY);
        let weak_requests_tx = requests_tx.downgrade();
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: name.clone(),
                })),
            })
            .await
            .map_err(|e| Error::Internal(e.into()))?;

        let (events, registration) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Disconnected),
            handshake = async {
                let mut events = client
                    .subscribe(ReceiverStream::new(requests_rx))
                    .await
                    .map_err(|s| routing_error(s, &name))?
                    .into_inner();
                let registration = expect_registration(&mut events)
                    .await
                    .map_err(|s| routing_error(s, &name))?;
                Ok::<_, Error>((events, registration))
            } => handshake?,
        };
        let node_ids = registration
            .node_ids
            .iter()
            .map(|s| s.parse::<NodeId>())
            .collect::<core::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Internal(e.into()))?;

        agent
            .on_register(
                Box::new(GrpcRoutingSink {
                    client,
                    token: Token::from(registration.session_token),
                    requests_tx,
                }),
                &node_ids,
            )
            .await;
        Ok((
            node_ids,
            Self {
                agent,
                cancel,
                requests_tx: weak_requests_tx,
                events,
            },
        ))
    }

    /// Holds the session open until its stream ends, fails, or is cancelled.
    ///
    /// The BPA sends a routing agent no events after the registration, so anything
    /// that arrives is logged and ignored. On the way out the agent's
    /// `on_unregister` runs and the session sends `Unregister`, so the BPA retires
    /// the token even when it was this side that ended the session.
    ///
    /// # Errors
    ///
    /// Returns the error the stream failed with, mapped as described on
    /// [`RegistrationHandle`](crate::client::RegistrationHandle).
    pub async fn handle_events(self) -> Result<()> {
        let Self {
            agent,
            cancel,
            requests_tx: weak_requests_tx,
            mut events,
        } = self;
        let result = loop {
            match next_event(&mut events, &cancel).await {
                ControlFlow::Continue(SubscribeResponse { event, .. }) => {
                    if event.is_some() {
                        warn!("ignoring unexpected event on the session stream");
                    }
                }
                ControlFlow::Break(None) => break Ok(()),
                ControlFlow::Break(Some(status)) => break Err(routing_session_error(status)),
            }
        };
        agent.on_unregister().await;

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
