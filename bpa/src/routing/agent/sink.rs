use hardy_eid_patterns::EidPattern;

use super::{Error, Result};
use crate::{
    Arc, async_trait,
    routing::{action::RouteAction, rib::Rib},
};

pub struct Sink {
    name: String,
    rib: Arc<Rib>,
}

impl Sink {
    pub fn new(name: String, rib: Arc<Rib>) -> Self {
        Self { name, rib }
    }

    fn check_connected(&self) -> Result<()> {
        if self.rib.has_agent(&self.name) {
            Ok(())
        } else {
            Err(Error::Disconnected)
        }
    }
}

// The trait stays path-qualified: this implementation struct is itself
// named `Sink`, and importing the trait would collide.
#[async_trait]
impl super::Sink for Sink {
    async fn unregister(&self) {
        self.rib.unregister_agent(&self.name).await;
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

impl Drop for Sink {
    fn drop(&mut self) {
        if self.rib.tasks.is_cancelled() || !self.rib.has_agent(&self.name) {
            return;
        }
        let name = self.name.clone();
        let rib = self.rib.clone();
        hardy_async::spawn!(self.rib.tasks, "routing_agent_drop_cleanup", async move {
            rib.unregister_agent(&name).await;
        });
    }
}
