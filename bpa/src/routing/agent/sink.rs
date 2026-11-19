use hardy_eid_patterns::EidPattern;

use crate::{
    async_trait,
    routing::{Result, RouteAction},
};

/// The channel an [`Agent`](super::Agent) pushes routes through.
///
/// The sink stamps the agent's registered name on every route as its
/// source, so an agent only ever adds or removes routes attributed to
/// itself. It arrives in the agent's [`Context`](super::Context) and
/// stays usable until the agent unregisters or the BPA shuts down, after
/// which every operation here returns
/// [`Error::Disconnected`](crate::routing::Error::Disconnected).
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the agent from the BPA, withdrawing the routes it added.
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
    /// Only a route matching the pattern, the action and the priority,
    /// and carrying this agent's source name, is removed. Returns whether
    /// one was found.
    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &RouteAction,
        priority: u32,
    ) -> Result<bool>;
}
