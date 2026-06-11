use alloc::sync::{Arc, Weak};

use crate::async_trait;

use super::Agent;
use super::rib::Rib;
use super::route::Route;
use super::{Error, Result, RoutingSink};

pub(super) struct Sink {
    agent: Weak<Agent>,
    rib: Arc<Rib>,
}

impl Sink {
    pub(super) fn new(agent: Weak<Agent>, rib: Arc<Rib>) -> Self {
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

    async fn update_routes(&self, add: &[Route], remove: &[Route]) -> Result<()> {
        let agent = self.agent.upgrade().ok_or(Error::Disconnected)?;
        self.rib
            .update_routes(&agent.name, add, remove)
            .await
            .map_err(|e| Error::Internal(e.into()))
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
