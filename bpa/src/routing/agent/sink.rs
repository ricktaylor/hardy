//! The interface a routing agent pushes routes through.

use hardy_eid_patterns::EidPattern;

use crate::{
    async_trait,
    routing::{Result, RouteAction},
};

/// The interface an [`Agent`](super::Agent) uses to push routes.
///
/// The sink stamps the agent's registered name on every route as its
/// source, so an agent can only add or remove routes attributed to
/// itself.
///
/// The sink is supplied in the [`Context`](super::Context) the agent is
/// built from, and stays usable until the agent unregisters or the BPA
/// shuts down, after which every method here returns
/// [`Error::Disconnected`](crate::routing::Error::Disconnected).
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the agent from the BPA, withdrawing the routes it
    /// added.
    async fn unregister(&self);

    /// Adds a route to the RIB.
    ///
    /// Returns `true` if the route was inserted, and `false` if an
    /// identical route was already present.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NullNextHop`] or [`Error::ViaOwnNode`] if the
    /// route's next hop is unusable, and [`Error::Disconnected`] once the
    /// agent has unregistered.
    ///
    /// [`Error::NullNextHop`]: crate::routing::Error::NullNextHop
    /// [`Error::ViaOwnNode`]: crate::routing::Error::ViaOwnNode
    /// [`Error::Disconnected`]: crate::routing::Error::Disconnected
    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool>;

    /// Removes a route from the RIB.
    ///
    /// Only a route matching the pattern, the action and the priority,
    /// and carrying this agent's source name, is removed. Returns `true`
    /// if one was found.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`Error::Disconnected`](crate::routing::Error::Disconnected) once
    /// the agent has unregistered.
    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &RouteAction,
        priority: u32,
    ) -> Result<bool>;
}
