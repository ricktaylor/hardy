use hardy_bpv7::eid::NodeId;
use tracing::error;

use crate::async_trait;

use super::{Route, RoutingAgent, RoutingSink};

/// A simple routing agent that installs a fixed set of routes on registration.
///
/// Routes are automatically removed when the agent is unregistered (via the BPA's
/// bulk cleanup). This is useful for tools and tests that need a quick set of
/// static routes without implementing the full [`RoutingAgent`] trait manually.
///
/// # Example
///
/// ```ignore
/// use hardy_bpa::routing::{StaticRoutingAgent, Route, RouteAction};
///
/// let agent = Arc::new(StaticRoutingAgent::new(&[
///     Route::via("ipn:*.*".parse().unwrap(), "ipn:0.2.0".parse().unwrap(), 30),
///     Route::drop("dtn://drop/**".parse().unwrap(), None, 50),
/// ]));
/// bpa.register_routing_agent("my_routes".to_string(), agent).await?;
/// ```
pub struct StaticRoutingAgent {
    routes: Vec<Route>,
    sink: hardy_async::sync::spin::Once<Box<dyn RoutingSink>>,
}

impl StaticRoutingAgent {
    pub fn new(routes: &[Route]) -> Self {
        Self {
            routes: routes.to_vec(),
            sink: hardy_async::sync::spin::Once::new(),
        }
    }
}

#[async_trait]
impl RoutingAgent for StaticRoutingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        if let Err(e) = sink.update_routes(&self.routes, &[]).await {
            error!("Failed to install static routes: {e}");
        }
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}
}
