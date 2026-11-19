use hardy_eid_patterns::EidPattern;
use tracing::warn;

use super::{Agent, Context, Factory, Sink, factory_fn};
use crate::{
    Arc, async_trait,
    routing::{Result, RouteAction},
};

/// An [`Agent`] that installs a fixed set of routes when it starts.
///
/// The BPA removes the routes when the agent unregisters. Tools and tests
/// that need a handful of static routes use this instead of implementing
/// [`Agent`].
///
/// # Example
///
/// ```ignore
/// use hardy_bpa::routing::{RegistrarExt, RouteAction, StaticAgent};
///
/// bpa.register_routing_agent(
///     "my_routes",
///     StaticAgent::factory(&[
///         ("ipn:*.*".parse().unwrap(), RouteAction::Via("ipn:0.2.0".parse().unwrap()), 30),
///         ("dtn://drop/**".parse().unwrap(), RouteAction::Drop(None), 50),
///     ]),
/// )
/// .await?;
/// ```
pub struct StaticAgent {
    routes: Vec<(EidPattern, RouteAction, u32)>,
    sink: Arc<dyn Sink>,
}

impl StaticAgent {
    /// Returns a factory for an agent that installs `routes`.
    pub fn factory(routes: &[(EidPattern, RouteAction, u32)]) -> impl Factory<Component = Self> {
        let routes = routes.to_vec();
        factory_fn(move |cx: Context| {
            Ok(Arc::new(Self {
                routes,
                sink: cx.sink,
            }))
        })
    }
}

#[async_trait]
impl Agent for StaticAgent {
    async fn register(&self) -> Result<()> {
        // A route the RIB rejects is logged and skipped; the rest are
        // still installed.
        for (pattern, action, priority) in &self.routes {
            if let Err(e) = self
                .sink
                .add_route(pattern.clone(), action.clone(), *priority)
                .await
            {
                warn!("Rejected route {pattern} => {action}: {e}");
            }
        }
        Ok(())
    }
}
