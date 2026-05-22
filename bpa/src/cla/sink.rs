use alloc::sync::{Arc, Weak};

use hardy_async::async_trait;
use hardy_bpv7::eid::NodeId;

use super::registry::{ClaEntry, ClaRegistry};
use crate::cla::{self, Bytes, ClaAddress, Sink};
use crate::dispatcher::Dispatcher;

pub struct ClaSink {
    cla: Weak<ClaEntry>,
    cla_name: Arc<str>,
    registry: Arc<ClaRegistry>,
    dispatcher: Arc<Dispatcher>,
}

impl ClaSink {
    pub(crate) fn new(
        cla_entry: &Arc<ClaEntry>,
        registry: Arc<ClaRegistry>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            cla: Arc::downgrade(cla_entry),
            cla_name: Arc::clone(&cla_entry.name),
            registry,
            dispatcher,
        }
    }

    fn cla(&self) -> cla::Result<Arc<ClaEntry>> {
        self.cla.upgrade().ok_or(cla::Error::Disconnected)
    }
}

#[async_trait]
impl Sink for ClaSink {
    async fn unregister(&self) {
        if let Some(cla) = self.cla.upgrade() {
            self.registry.unregister(&cla.name).await;
        }
    }

    async fn dispatch(
        &self,
        bundle: Bytes,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&ClaAddress>,
    ) -> cla::Result<()> {
        self.cla()?;
        self.dispatcher
            .receive_bundle(
                bundle,
                Some(self.cla_name.clone()),
                peer_node.cloned(),
                peer_addr.cloned(),
            )
            .await
    }

    async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> cla::Result<bool> {
        let cla = self.cla()?;
        Ok(self
            .registry
            .add_peer(cla, self.dispatcher.clone(), cla_addr, node_ids)
            .await)
    }

    async fn remove_peer(&self, cla_addr: &ClaAddress) -> cla::Result<bool> {
        let cla = self.cla()?;
        Ok(self.registry.remove_peer(cla, cla_addr).await)
    }
}

impl Drop for ClaSink {
    fn drop(&mut self) {
        self.registry.signal_dropped(self.cla.clone());
    }
}
