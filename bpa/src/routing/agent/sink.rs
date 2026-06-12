use alloc::sync::Arc;

use crate::async_trait;
use crate::routing::rib::Rib;
use crate::routing::route::Route;

use super::{Error, Result, RoutingSink};

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

    async fn update_routes(&self, add: &[Route], remove: &[Route]) -> Result<()> {
        self.rib
            .update_routes(&self.name, add, remove)
            .await
            .map_err(|e| Error::Internal(e.into()))
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
