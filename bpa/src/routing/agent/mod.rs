use hardy_bpv7::eid::{Eid, NodeId};
use hardy_eid_patterns::EidPattern;
use thiserror::Error;

use super::action::RouteAction;
use crate::async_trait;

mod static_agent;

pub(crate) mod sink;

pub use self::static_agent::StaticAgent;

/// A specialized `Result` type for routing agent operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during routing agent operations.
#[derive(Debug, Error)]
pub enum Error {
    /// An attempt was made to register a routing agent with a name already in use.
    #[error("Attempt to register duplicate routing agent name {0}")]
    AlreadyExists(String),

    /// The connection to the BPA has been lost.
    #[error("The sink is disconnected")]
    Disconnected,

    /// A route was rejected because the next-hop is the null endpoint.
    #[error("Route via null next-hop rejected")]
    NullNextHop,

    /// A route was rejected because the next-hop resolves to the local node.
    #[error("Route via own node {0} rejected")]
    ViaOwnNode(Eid),

    /// An internal error occurred.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

/// A component that supplies routes to the BPA.
///
/// An agent discovers or computes routes and pushes them into the BPA's
/// routing information base through a [`Sink`]: static configuration, a
/// link-state protocol and neighbour discovery are all agents.
///
/// Agents are purely push-based. The BPA never calls back to ask for
/// work, unlike a [`Cla`](crate::cla::Cla), which is asked to forward.
///
/// # Sink Lifecycle
///
/// The agent receives a [`Sink`] in [`on_register`](Self::on_register)
/// and must store it for its entire active lifetime: dropping the sink is
/// how an agent asks to be disconnected, so `on_register` has to store it
/// before returning, or the BPA immediately calls
/// [`on_unregister`](Self::on_unregister).
///
/// Disconnection runs in either direction. Agent-initiated, the agent
/// drops its sink or calls `sink.unregister()`, and the BPA calls
/// `on_unregister`. BPA-initiated, the BPA shuts down, calls
/// `on_unregister`, and the sink stops functioning. Either way the BPA
/// removes every route the agent added.
#[async_trait]
pub trait Agent: Send + Sync {
    /// Called when the agent is registered with the BPA.
    ///
    /// The `sink` must be stored for the agent's entire active lifetime;
    /// dropping it triggers unregistration. `node_ids` are the BPA's own
    /// node identifiers.
    async fn on_register(&self, sink: Box<dyn Sink>, node_ids: &[NodeId]);

    /// Called when the agent is being unregistered, either because the
    /// agent dropped its sink or because the BPA is shutting down.
    ///
    /// The agent releases what it holds here — background tasks, open
    /// connections; the BPA removes the agent's routes once this
    /// returns.
    async fn on_unregister(&self);
}

/// The channel an [`Agent`] pushes routes through.
///
/// The sink stamps the agent's registered name on every route as its
/// source, so an agent only ever adds or removes routes attributed to
/// itself.
///
/// This is handed to [`Agent::on_register`] and must be stored for the
/// agent's entire active lifetime.
///
/// # Lifecycle
///
/// Dropping this sink is how an agent unregisters: the BPA detects the
/// drop and calls [`Agent::on_unregister`]. When the BPA shuts down
/// instead, it calls [`Agent::on_unregister`] first, after which every
/// operation here returns [`Error::Disconnected`].
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the associated agent from the BPA.
    ///
    /// Equivalent to dropping the sink, but lets the agent choose when
    /// the teardown happens.
    async fn unregister(&self);

    /// Adds a route to the RIB.
    ///
    /// Returns whether the route was newly inserted; an identical route
    /// already in the RIB returns `false`.
    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool>;

    /// Removes a route from the RIB.
    ///
    /// Only a route matching the pattern, the action and the priority —
    /// and carrying this agent's source name — is removed. Returns
    /// whether one was found.
    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &RouteAction,
        priority: u32,
    ) -> Result<bool>;
}
