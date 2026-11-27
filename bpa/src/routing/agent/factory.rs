//! Constructing a routing agent for the node the BPA runs as.

use core::fmt::{self, Debug, Formatter};

use alloc::boxed::Box;

use super::{Result, RoutingAgent, RoutingSink};
use crate::{Arc, node_ids::NodeIds};

/// Everything the BPA hands an [`AgentFactory`] when it builds the agent.
pub struct AgentContext {
    /// The channel back to the BPA's RIB.
    ///
    /// The same channel [`RoutingAgent::on_register`] delivers: an agent
    /// keeps whichever handle suits it, and unregisters when it drops the
    /// last of them. The sink is not live until the build returns, so
    /// [`AgentFactory::build`] must not use it.
    pub sink: Arc<dyn RoutingSink>,

    /// The BPA's own node identifiers, which the routes an agent installs
    /// are relative to.
    pub node_ids: Arc<NodeIds>,
}

impl AgentContext {
    /// Mints a context. The BPA does this; an agent's tests mint one to
    /// build the agent for the node they exercise.
    pub fn new(sink: Arc<dyn RoutingSink>, node_ids: Arc<NodeIds>) -> Self {
        Self { sink, node_ids }
    }
}

impl Debug for AgentContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentContext")
            .field("node_ids", &self.node_ids)
            .finish_non_exhaustive()
    }
}

/// Builds a [`RoutingAgent`] when the BPA registers it.
///
/// [`BpaBuilder::routing_agent`](crate::builder::BpaBuilder::routing_agent)
/// takes a factory, and the BPA runs it with the node ids and the sink in
/// hand; then [`RoutingAgent::on_register`] runs as for any agent. An
/// already built agent is a factory whose `build` returns it.
pub trait AgentFactory: Send {
    /// Builds the agent for the node the BPA runs as.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of `cx.sink`.
    /// `Err` means the agent cannot route for this node.
    fn build(self, cx: AgentContext) -> Result<Arc<dyn RoutingAgent>>;
}

impl<A: RoutingAgent + 'static> AgentFactory for Arc<A> {
    /// Returns the agent, which takes its sink in
    /// [`RoutingAgent::on_register`].
    fn build(self, _cx: AgentContext) -> Result<Arc<dyn RoutingAgent>> {
        Ok(self)
    }
}

impl AgentFactory for Arc<dyn RoutingAgent> {
    /// Returns the agent, which takes its sink in
    /// [`RoutingAgent::on_register`].
    fn build(self, _cx: AgentContext) -> Result<Arc<dyn RoutingAgent>> {
        Ok(self)
    }
}

// An agent factory with its type erased, so the RIB holds factories of
// different types.
pub struct BoxFactory(Box<dyn FnOnce(AgentContext) -> Result<Arc<dyn RoutingAgent>> + Send>);

impl BoxFactory {
    pub fn new<F: AgentFactory + 'static>(factory: F) -> Self {
        Self(Box::new(|cx| factory.build(cx)))
    }

    pub fn build(self, cx: AgentContext) -> Result<Arc<dyn RoutingAgent>> {
        (self.0)(cx)
    }
}
