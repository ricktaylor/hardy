use hardy_async::async_trait;
use hardy_bpv7::eid::NodeId;

use super::adapter::Adapter;
use super::engine::ClaEngine;
use super::{ClaAddress, Result};
use crate::dispatcher::Dispatcher;
use crate::{Arc, Bytes, Weak};

pub(super) struct ClaCallback {
    pub cla: Weak<Adapter>,
    pub engine: Arc<ClaEngine>,
    pub dispatcher: Arc<Dispatcher>,
}

#[async_trait]
impl super::Sink for ClaCallback {
    async fn unregister(&self) {
        if let Some(cla) = self.cla.upgrade() {
            self.engine.unregister(cla).await;
        }
    }

    async fn dispatch(
        &self,
        bundle: Bytes,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&ClaAddress>,
    ) -> Result<()> {
        let cla = self.cla.upgrade().ok_or(super::Error::Disconnected)?;
        self.dispatcher
            .receive_bundle(
                bundle,
                Some(cla.name.clone()),
                peer_node.cloned(),
                peer_addr.cloned(),
            )
            .await
    }

    async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> Result<bool> {
        let cla = self.cla.upgrade().ok_or(super::Error::Disconnected)?;
        Ok(self.engine.add_peer(cla, cla_addr, node_ids).await)
    }

    async fn remove_peer(&self, cla_addr: &ClaAddress) -> Result<bool> {
        Ok(self
            .engine
            .remove_peer(
                self.cla.upgrade().ok_or(super::Error::Disconnected)?,
                cla_addr,
            )
            .await)
    }
}

impl Drop for ClaCallback {
    fn drop(&mut self) {
        if let Some(cla) = self.cla.upgrade() {
            let engine = self.engine.clone();
            hardy_async::spawn!(self.engine.tasks, "cla_drop_cleanup", async move {
                engine.unregister(cla).await;
            });
        }
    }
}
