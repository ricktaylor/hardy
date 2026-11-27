use super::*;

// One registration's channel back to the BPA, shared by the factory's
// `Context` and the handle `on_register` receives. The registration it
// serves is filed only once the CLA is built, so the back-reference is set
// then; until it is, the sink answers `Disconnected`.
pub struct Sink {
    pub cla: Once<Weak<Cla>>,
    pub registry: Arc<ClaRegistry>,
    pub dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Sink {
    fn cla(&self) -> Option<Arc<Cla>> {
        self.cla.get().and_then(Weak::upgrade)
    }
}

#[async_trait]
impl cla::Sink for Sink {
    async fn unregister(&self) {
        if let Some(cla) = self.cla() {
            self.registry.unregister(cla).await;
        }
    }

    async fn dispatch(
        &self,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&ClaAddress>,
        stream: &mut dyn crate::stream::Receiver<Segment>,
    ) -> Result<cla::Acceptance> {
        let cla = self.cla().ok_or(cla::Error::Disconnected)?;

        // A CLA that unregisters mid-stream must not land its bundle: the
        // registration's token races every pull, so teardown wakes this
        // stream immediately — even parked behind a stalled producer — and
        // the transfer surfaces as cancelled. Sink-side, so the dispatcher's
        // stream consumers stay registration-agnostic.
        let mut stream = crate::stream::CancellableReceiver {
            inner: stream,
            token: cla.cancel.clone(),
        };
        let verdict = self
            .dispatcher
            .receive_bundle(cla.name.clone(), peer_node, peer_addr, &mut stream)
            .await;

        // A refusal caused by that teardown is not a verdict on the bundle
        // — the registration died. Report the dead sink instead, so the CLA
        // does not mistake its own unregistration for a per-bundle refusal.
        if matches!(verdict, cla::Acceptance::Refused) && cla.cancel.is_cancelled() {
            return Err(cla::Error::Disconnected);
        }
        Ok(verdict)
    }

    async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> cla::Result<bool> {
        let cla = self.cla().ok_or(cla::Error::Disconnected)?;
        Ok(self
            .registry
            .add_peer(cla, self.dispatcher.clone(), cla_addr, node_ids)
            .await)
    }

    async fn remove_peer(&self, cla_addr: &ClaAddress) -> cla::Result<bool> {
        Ok(self
            .registry
            .remove_peer(self.cla().ok_or(cla::Error::Disconnected)?, cla_addr)
            .await)
    }

    async fn transfer_outcome(
        &self,
        bundle_id: &Id,
        outcome: cla::TransferOutcome,
    ) -> cla::Result<()> {
        let cla = self.cla().ok_or(cla::Error::Disconnected)?;
        self.dispatcher
            .transfer_outcome(&cla, bundle_id, outcome)
            .await;
        Ok(())
    }
}

// The last handle gone is the CLA asking to leave.
impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(cla) = self.cla() {
            let registry = self.registry.clone();
            hardy_async::spawn!(self.registry.tasks, "cla_drop_cleanup", async move {
                registry.unregister(cla).await;
            });
        }
    }
}

// The handle `on_register` receives: one more owner of the shared sink.
pub struct SinkHandle(pub Arc<Sink>);

#[async_trait]
impl cla::Sink for SinkHandle {
    async fn unregister(&self) {
        self.0.unregister().await
    }

    async fn dispatch(
        &self,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&ClaAddress>,
        stream: &mut dyn crate::stream::Receiver<Segment>,
    ) -> Result<cla::Acceptance> {
        self.0.dispatch(peer_node, peer_addr, stream).await
    }

    async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> cla::Result<bool> {
        self.0.add_peer(cla_addr, node_ids).await
    }

    async fn remove_peer(&self, cla_addr: &ClaAddress) -> cla::Result<bool> {
        self.0.remove_peer(cla_addr).await
    }

    async fn transfer_outcome(
        &self,
        bundle_id: &Id,
        outcome: cla::TransferOutcome,
    ) -> cla::Result<()> {
        self.0.transfer_outcome(bundle_id, outcome).await
    }
}
