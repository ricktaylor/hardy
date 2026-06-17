pub(crate) mod sink;
mod static_agent;

pub use static_agent::StaticRoutingAgent;

use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use thiserror::Error;
use tracing::warn;

use crate::async_trait;

use crate::{Arc, async_trait};

/// A specialized `Result` type for routing agent operations.
pub type Result<T> = core::result::Result<T, self::Error>;

pub(crate) struct Agent {
    pub(super) agent: Arc<dyn RoutingAgent>,
    pub(crate) name: String,
}

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

/// The action to take when a route matches a bundle's destination.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    /// Drop the bundle, optionally reporting a reason code.
    Drop(Option<ReasonCode>),
    /// Return the bundle to the previous hop (last-hop reflection).
    Reflect,
    /// Forward the bundle via the specified next-hop EID (recursive lookup).
    Via(Eid),
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Drop(Some(reason)) => write!(f, "Drop({reason:?})"),
            Action::Drop(None) => write!(f, "Drop"),
            Action::Reflect => write!(f, "Reflect"),
            Action::Via(eid) => write!(f, "Via {eid}"),
        }
    }
}

/// The primary trait for a Routing Agent.
///
/// A routing agent discovers or computes routes and pushes them to the BPA's
/// Routing Information Base (RIB) via a [`RoutingSink`]. Examples include static
/// route configuration, link-state protocols, and neighbour discovery.
///
/// Routing agents are purely push-based: they push routes to the BPA via the Sink,
/// and the BPA never calls back into the agent to request work (unlike CLAs which
/// have a `forward` method).
///
/// # Sink Lifecycle
///
/// The routing agent receives a [`RoutingSink`] in [`on_register`](Self::on_register)
/// which it **must store** for its entire active lifetime. The Sink provides the
/// communication channel back to the BPA's RIB.
///
/// **Critical**: If the Sink is dropped (either explicitly or by not storing it), the BPA
/// interprets this as the agent requesting disconnection and will call
/// [`on_unregister`](Self::on_unregister). All routes from this agent are automatically removed.
///
/// Two disconnection paths exist:
/// - **Agent-initiated**: Agent drops its Sink or calls `sink.unregister()` -> BPA calls `on_unregister()`
/// - **BPA-initiated**: BPA shuts down -> calls `on_unregister()` -> Sink becomes non-functional
#[async_trait]
pub trait RoutingAgent: Send + Sync {
    /// Called when the routing agent is registered with the BPA.
    ///
    /// The agent should store the `sink` for its entire active lifetime.
    /// Dropping the sink triggers automatic unregistration and route cleanup.
    ///
    /// # Arguments
    /// * `sink` - Communication channel back to the BPA's RIB. Must be stored.
    /// * `node_ids` - The BPA's own node identifiers.
    async fn on_register(&self, sink: Box<dyn RoutingSink>, node_ids: &[NodeId]);

    /// Called when the routing agent is being unregistered.
    ///
    /// Called when either:
    /// 1. The agent dropped its Sink (agent-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    ///
    /// The agent should perform cleanup: stop background tasks, close connections,
    /// and release resources. Routes are automatically removed by the BPA after this returns.
    async fn on_unregister(&self);
}

/// A communication channel from a routing agent back to the BPA's RIB.
///
/// The Sink automatically injects the agent's registered name as the route source,
/// so an agent can only add/remove routes attributed to itself.
///
/// # Lifecycle
///
/// The Sink is provided in [`RoutingAgent::on_register`]. The agent **must store** this
/// Sink for its entire active lifetime. When the Sink is dropped, the BPA interprets
/// this as the agent requesting disconnection.
///
/// After disconnection, all Sink operations return [`Error::Disconnected`].
#[async_trait]
pub trait RoutingSink: Send + Sync {
    /// Explicitly unregisters the associated routing agent from the BPA.
    ///
    /// Equivalent to dropping the Sink. After this call, the BPA calls
    /// [`RoutingAgent::on_unregister`] and all routes from this agent are removed.
    async fn unregister(&self);

    /// Adds a route to the RIB.
    ///
    /// Returns `true` if the route was newly inserted, `false` if it already existed.
    async fn add_route(&self, pattern: EidPattern, action: Action, priority: u32) -> Result<bool>;

    /// Removes a specific route from the RIB.
    ///
    /// Only removes routes that match all three fields (pattern, action, priority)
    /// and this agent's source name.
    ///
    /// Returns `true` if the route was found and removed.
    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &Action,
        priority: u32,
    ) -> Result<bool>;
}
