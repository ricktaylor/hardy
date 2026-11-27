use hardy_async::sync::spin::Once;
use hardy_eid_patterns::EidPattern;

use super::{Error, Result, RoutingSink};
use crate::{
    Arc, async_trait,
    routing::{action::RouteAction, rib::Rib},
};

// One registration's channel back to the RIB, shared by the factory's
// context and the handle `on_register` receives. It acts under the agent's
// name only once that agent is filed: the sink of a build refused as a
// duplicate must never touch the agent already holding the name.
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
