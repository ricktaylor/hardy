use hardy_async::sync::spin::Once;
use hardy_eid_patterns::EidPattern;

use super::{Error, Result, RoutingSink};
use crate::{
    Arc, async_trait,
    routing::{action::RouteAction, rib::Rib},
};

// One registration's channel back to the RIB, shared by the factory's
// context and the handle `on_register` receives. It acts under the agent's
// name only once that agent is filed: the sink of a build that failed must
// never touch an agent registered under the name afterwards.
pub struct Sink {
    name: String,
    rib: Arc<Rib>,
    filed: Once<()>,
}

impl Sink {
    pub fn new(name: String, rib: Arc<Rib>) -> Self {
        Self {
            name,
            rib,
            filed: Once::new(),
        }
    }

    // Marks the agent as filed under the sink's name.
    pub fn file(&self) {
        self.filed.call_once(|| ());
    }

    fn connected(&self) -> bool {
        self.filed.is_completed() && self.rib.has_agent(&self.name)
    }

    fn check_connected(&self) -> Result<()> {
        if self.connected() {
            Ok(())
        } else {
            Err(Error::Disconnected)
        }
    }
}

#[async_trait]
impl RoutingSink for Sink {
    async fn unregister(&self) {
        if self.filed.is_completed() {
            self.rib.unregister_agent(&self.name).await;
        }
    }

    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool> {
        self.check_connected()?;
        self.rib
            .add(pattern, self.name.clone(), action.into(), priority)
            .await
    }

    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &RouteAction,
        priority: u32,
    ) -> Result<bool> {
        self.check_connected()?;
        Ok(self
            .rib
            .remove(pattern, &self.name, action.clone().into(), priority)
            .await)
    }
}

// The last handle gone is the agent asking to leave.
impl Drop for Sink {
    fn drop(&mut self) {
        if self.rib.tasks.is_cancelled() || !self.connected() {
            return;
        }
        let name = self.name.clone();
        let rib = self.rib.clone();
        hardy_async::spawn!(self.rib.tasks, "routing_agent_drop_cleanup", async move {
            rib.unregister_agent(&name).await;
        });
    }
}

// The handle `on_register` receives: one more owner of the shared sink.
pub struct SinkHandle(pub Arc<Sink>);

#[async_trait]
impl RoutingSink for SinkHandle {
    async fn unregister(&self) {
        self.0.unregister().await
    }

    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool> {
        self.0.add_route(pattern, action, priority).await
    }

    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &RouteAction,
        priority: u32,
    ) -> Result<bool> {
        self.0.remove_route(pattern, action, priority).await
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::eid::NodeId;

    use super::*;
    use crate::{
        bpa::Bpa,
        routing::{AgentContext, AgentFactory, RoutingAgent, agent::factory::BoxFactory},
    };

    /// Keeps its sink once registered.
    struct TestAgent {
        sink: Once<Box<dyn RoutingSink>>,
    }

    #[async_trait]
    impl RoutingAgent for TestAgent {
        async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}
    }

    /// Keeps the context's sink, then refuses to build.
    struct RefusingFactory {
        sink_tx: flume::Sender<Arc<dyn RoutingSink>>,
    }

    impl AgentFactory for RefusingFactory {
        fn build(self, cx: AgentContext) -> Result<Arc<dyn RoutingAgent>> {
            let _ = self.sink_tx.send(cx.sink);
            Err(Error::Disconnected)
        }
    }

    // A factory that fails releases its name, and the sink it kept never
    // acts for, or unregisters, the agent registered under the name next.
    #[tokio::test]
    async fn a_failed_build_frees_the_name_and_its_sink_stays_dead() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;
        let rib = bpa.rib();

        let (sink_tx, sink_rx) = flume::bounded(1);
        let result = rib
            .register_agent("agent".into(), BoxFactory::new(RefusingFactory { sink_tx }))
            .await;
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
        let stale = sink_rx.try_recv().expect("the factory ran");

        let agent = Arc::new(TestAgent { sink: Once::new() });
        rib.register_agent("agent".into(), BoxFactory::new(agent.clone()))
            .await
            .expect("the failed build released the name");

        let pattern: EidPattern = "ipn:0.9.*".parse().unwrap();
        assert!(matches!(
            stale
                .add_route(pattern.clone(), RouteAction::Drop(None), 10)
                .await,
            Err(Error::Disconnected)
        ));
        stale.unregister().await;
        let live = agent.sink.get().expect("on_register ran");
        assert!(
            live.add_route(pattern, RouteAction::Drop(None), 10)
                .await
                .unwrap(),
            "the stale sink's unregister must leave the new agent registered"
        );

        bpa.shutdown().await;
    }
}
