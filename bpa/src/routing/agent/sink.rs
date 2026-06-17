use hardy_eid_patterns::EidPattern;

use super::{Agent, Error, Result, RoutingSink};
use crate::routing::action::RouteAction;
use crate::routing::rib::Rib;
use crate::{Arc, Weak, async_trait};

pub struct Sink {
    agent: Weak<Agent>,
    rib: Arc<Rib>,
}

impl Sink {
    pub fn new(agent: Weak<Agent>, rib: Arc<Rib>) -> Self {
        Self { agent, rib }
    }
}

#[async_trait]
impl RoutingSink for Sink {
    async fn unregister(&self) {
        if let Some(agent) = self.agent.upgrade() {
            self.rib.unregister_agent(agent).await;
        }
    }

    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool> {
        let agent = self.agent.upgrade().ok_or(Error::Disconnected)?;
        Ok(self
            .rib
            .add(pattern, agent.name.clone(), action.into(), priority)
            .await)
    }

    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &RouteAction,
        priority: u32,
    ) -> Result<bool> {
        let agent = self.agent.upgrade().ok_or(Error::Disconnected)?;
        Ok(self
            .rib
            .remove(pattern, &agent.name, action.clone().into(), priority)
            .await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.upgrade() {
            let rib = self.rib.clone();
            hardy_async::spawn!(self.rib.tasks, "routing_agent_drop_cleanup", async move {
                rib.unregister_agent(agent).await;
            });
        }
    }
}
