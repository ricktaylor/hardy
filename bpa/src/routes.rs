use alloc::string::String;
use alloc::vec::Vec;

use hardy_async::async_trait;
use hardy_bpv7::eid::{Eid, NodeId};
use hardy_bpv7::status_report::ReasonCode;
use hardy_eid_patterns::EidPattern;
use thiserror::Error;

pub use crate::rib::context::{RouteOp, RoutingContext};

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

impl core::fmt::Display for Action {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Action::Drop(Some(reason)) => write!(f, "Drop({reason:?})"),
            Action::Drop(None) => write!(f, "Drop"),
            Action::Reflect => write!(f, "Reflect"),
            Action::Via(eid) => write!(f, "Via {eid}"),
        }
    }
}

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

    /// An internal error occurred.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

/// The primary trait for a Routing Agent.
///
/// A routing agent discovers or computes routes and pushes them to the BPA's
/// Routing Information Base (RIB) via a [`RoutingContext`]. Examples include static
/// route configuration, link-state protocols, and neighbour discovery.
///
/// # Context Lifecycle
///
/// The routing agent receives a [`RoutingContext`] in
/// [`on_register`](Self::on_register). The context contains channel senders
/// for adding and removing routes. The agent should clone and store the
/// context if it needs to manage routes beyond initialization.
///
/// Dropping all clones of the context closes the channels, which the BPA
/// detects as disconnection. All routes from this agent are automatically removed.
///
/// Two disconnection paths exist:
/// - **Agent-initiated**: Agent drops all RoutingContext clones. BPA calls `on_unregister()`.
/// - **BPA-initiated**: BPA cancels the shutdown token. Agent should stop work and drop the context.
#[async_trait]
pub trait RoutingAgent: Send + Sync {
    /// Called when the routing agent is registered with the BPA.
    ///
    /// The `ctx` provides channel-based access to the RIB for adding and
    /// removing routes. Clone it if you need it beyond this call.
    ///
    /// # Arguments
    /// * `ctx` - Channel-based context for managing routes in the RIB.
    /// * `node_ids` - The BPA's own node identifiers.
    async fn on_register(&self, ctx: RoutingContext, node_ids: &[NodeId]);

    /// Called when the routing agent is being unregistered.
    ///
    /// Called when either:
    /// 1. All context clones were dropped (agent-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    ///
    /// Routes are automatically removed by the BPA after this returns.
    async fn on_unregister(&self);
}

/// A simple routing agent that installs a fixed set of routes on registration.
///
/// Routes are automatically removed when the agent is unregistered (via the BPA's
/// bulk cleanup). This is useful for tools and tests that need a quick set of
/// static routes without implementing the full [`RoutingAgent`] trait manually.
///
/// # Example
///
/// ```ignore
/// use hardy_bpa::routes::StaticRoutingAgent;
/// use hardy_bpa::routes::Action;
///
/// let agent = Arc::new(StaticRoutingAgent::new(&[
///     ("ipn:*.*".parse().unwrap(), Action::Via("ipn:0.2.0".parse().unwrap()), 30),
///     ("dtn://drop/**".parse().unwrap(), Action::Drop(None), 50),
/// ]));
/// bpa.register_routing_agent("my_routes".to_string(), agent).await?;
/// ```
pub struct StaticRoutingAgent {
    routes: Vec<(EidPattern, Action, u32)>,
    ctx: hardy_async::sync::spin::Once<RoutingContext>,
}

impl StaticRoutingAgent {
    /// Creates a new static routing agent with the given set of routes.
    pub fn new(routes: &[(EidPattern, Action, u32)]) -> Self {
        Self {
            routes: routes.to_vec(),
            ctx: hardy_async::sync::spin::Once::new(),
        }
    }
}

#[async_trait]
impl RoutingAgent for StaticRoutingAgent {
    async fn on_register(&self, ctx: RoutingContext, _node_ids: &[NodeId]) {
        for (pattern, action, priority) in &self.routes {
            ctx.add_route(pattern.clone(), action.clone(), *priority);
        }
        self.ctx.call_once(|| ctx);
    }

    async fn on_unregister(&self) {}
}
