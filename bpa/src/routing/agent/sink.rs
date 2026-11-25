use hardy_eid_patterns::EidPattern;

use super::{Error, Result};
use crate::{
    Arc, Weak, async_trait,
    routing::{action::RouteAction, rib::Rib},
};

/// The back-edge is weak. `Bpa` owns the RIB for the life of the BPA, so a
/// strong edge would add no reachability — only a cycle through the agent
/// that holds this sink, and a cycle of `Arc`s never drops. A failed
/// upgrade is the condition the trait already names, [`Error::Disconnected`].
pub struct Sink {
    name: String,
    rib: Weak<Rib>,
}

impl Sink {
    pub fn new(name: String, rib: Weak<Rib>) -> Self {
        Self { name, rib }
    }

    /// The RIB this sink speaks for — [`Error::Disconnected`] once the BPA
    /// is gone, or the agent has unregistered.
    fn rib(&self) -> Result<Arc<Rib>> {
        let rib = self.rib.upgrade().ok_or(Error::Disconnected)?;
        if rib.has_agent(&self.name) {
            Ok(rib)
        } else {
            Err(Error::Disconnected)
        }
    }
}

// This module's `Sink` is the implementation the BPA hands out, so the
// trait it serves stays qualified.
#[async_trait]
impl super::Sink for Sink {
    async fn unregister(&self) {
        if let Some(rib) = self.rib.upgrade() {
            rib.unregister_agent(&self.name).await;
        }
    }

    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool> {
        self.rib()?
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
            .rib()?
            .remove(pattern, &self.name, action.clone().into(), priority)
            .await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let Some(rib) = self.rib.upgrade() else {
            return;
        };
        if rib.tasks.is_cancelled() || !rib.has_agent(&self.name) {
            return;
        }
        let name = self.name.clone();
        let tasks = rib.tasks.clone();
        hardy_async::spawn!(tasks, "routing_agent_drop_cleanup", async move {
            rib.unregister_agent(&name).await;
        });
    }
}
