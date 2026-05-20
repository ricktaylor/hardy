use super::*;
use hardy_bpa::async_trait;
use hardy_bpa::cla::ClaContext;

#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    async fn on_register(&self, ctx: ClaContext, _node_ids: &[NodeId]) {
        for (eid, path) in &self.inboxes {
            ctx.add_peer(
                hardy_bpa::cla::ClaAddress::Private(hardy_bpa::Bytes::copy_from_slice(
                    path.as_bytes(),
                )),
                vec![eid.clone()],
            );
        }

        let ctx = self.ctx.call_once(|| ctx);

        if let Some(outbox) = &self.outbox {
            self.start_watcher(ctx.clone(), outbox.clone()).await;
        }
    }

    async fn on_unregister(&self) {
        self.tasks.shutdown().await;
    }

    async fn forward(
        &self,
        _queue: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        if !self.ctx.get().is_some_and(|c| c.is_connected()) {
            error!("forward called before on_register!");
            return Err(hardy_bpa::cla::Error::Disconnected);
        }

        if let hardy_bpa::cla::ClaAddress::Private(remote_addr) = cla_addr
            && let Ok(addr_str) = str::from_utf8(remote_addr.as_ref())
            && self.inboxes.values().any(|p| p == addr_str)
        {
            let path = match hardy_bpv7::bundle::Id::parse(&bundle) {
                Ok(id) => {
                    let mut filename = format!("{}_{}", id.source, id.timestamp)
                        .replace(['\\', '/', ':', ' '], "_");
                    if let Some(fragment_info) = id.fragment_info {
                        filename.push_str(format!("_fragment_{}", fragment_info.offset).as_str());
                    }
                    PathBuf::from(addr_str).join(filename)
                }
                Err(e) => {
                    warn!("Ignoring invalid bundle: {e}");
                    return Err(hardy_bpa::cla::Error::Internal(e.into()));
                }
            };

            return tokio::fs::write(&path, bundle)
                .await
                .map(|_| hardy_bpa::cla::ForwardBundleResult::Sent)
                .map_err(|e| {
                    error!("Failed to write to '{}': {e}", path.display());
                    hardy_bpa::cla::Error::Internal(e.into())
                });
        }

        Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour)
    }
}
