use hardy_async::sync::spin;
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use tracing::warn;

use super::{Agent, Sink};
use crate::{async_trait, routing::action::RouteAction};

/// An [`Agent`] that installs a fixed set of routes on registration.
///
/// The BPA removes the routes when the agent unregisters. Tools and
/// tests that need a handful of static routes use this instead of
/// implementing [`Agent`].
///
/// # Example
///
/// ```ignore
/// use hardy_bpa::routing::{RouteAction, StaticAgent};
///
/// let agent = Arc::new(StaticAgent::new(&[
///     ("ipn:*.*".parse().unwrap(), RouteAction::Via("ipn:0.2.0".parse().unwrap()), 30),
///     ("dtn://drop/**".parse().unwrap(), RouteAction::Drop(None), 50),
/// ]));
/// bpa.register_routing_agent("my_routes".to_string(), agent).await?;
/// ```
pub struct StaticAgent {
    routes: Vec<(EidPattern, RouteAction, u32)>,
    sink: spin::Once<Box<dyn Sink>>,
}

impl StaticAgent {
    pub fn new(routes: &[(EidPattern, RouteAction, u32)]) -> Self {
        Self {
            routes: routes.to_vec(),
            sink: spin::Once::new(),
        }
    }
}

#[async_trait]
impl Agent for StaticAgent {
    async fn on_register(&self, sink: Box<dyn Sink>, _node_ids: &[NodeId]) {
        for (pattern, action, priority) in &self.routes {
            if let Err(e) = sink
                .add_route(pattern.clone(), action.clone(), *priority)
                .await
            {
                warn!("Rejected route {pattern} => {action}: {e}");
            }
        }
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}
}
