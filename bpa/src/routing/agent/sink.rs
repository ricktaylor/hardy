use hardy_eid_patterns::EidPattern;

use super::{Result, RoutingSink};
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
}

#[async_trait]
impl RoutingSink for Sink {
    async fn unregister(&self) {
        self.rib.unregister_agent(&self.name).await;
    }

    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool> {
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
        Ok(self
            .rib
            .remove(pattern, &self.name, action.clone().into(), priority)
            .await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let name = self.name.clone();
        let rib = self.rib.clone();
        hardy_async::spawn!(self.rib.tasks, "routing_agent_drop_cleanup", async move {
            rib.unregister_agent(&name).await;
        });
    }
}
