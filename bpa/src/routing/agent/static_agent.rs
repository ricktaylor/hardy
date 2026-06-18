use hardy_async::sync::spin;
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;

use super::{RoutingAgent, RoutingSink};
use crate::async_trait;
use crate::routing::action::RouteAction;

/// A simple routing agent that installs a fixed set of routes on registration.
///
/// Routes are automatically removed when the agent is unregistered (via the BPA's
/// bulk cleanup). This is useful for tools and tests that need a quick set of
/// static routes without implementing the full [`RoutingAgent`] trait manually.
///
/// # Example
///
/// ```ignore
/// use hardy_bpa::routing::{StaticRoutingAgent, RouteAction};
///
/// let agent = Arc::new(StaticRoutingAgent::new(&[
///     ("ipn:*.*".parse().unwrap(), RouteAction::Via("ipn:0.2.0".parse().unwrap()), 30),
///     ("dtn://drop/**".parse().unwrap(), RouteAction::Drop(None), 50),
/// ]));
/// bpa.register_routing_agent("my_routes".to_string(), agent).await?;
/// ```
pub struct StaticRoutingAgent {
    routes: Vec<(EidPattern, RouteAction, u32)>,
    sink: spin::Once<Box<dyn RoutingSink>>,
}

impl StaticRoutingAgent {
    pub fn new(routes: &[(EidPattern, RouteAction, u32)]) -> Self {
        Self {
            routes: routes.to_vec(),
            sink: spin::Once::new(),
        }
    }
}

#[async_trait]
impl RoutingAgent for StaticRoutingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        for (pattern, action, priority) in &self.routes {
            sink.add_route(pattern.clone(), action.clone(), *priority)
                .await
                .ok();
        }
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}
}
