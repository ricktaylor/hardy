use std::{path::PathBuf, slice};

use hardy_bpa::{
    Bytes, async_trait, cla,
    stream::{Receiver, buffer_stream},
};
use hardy_bpv7::bundle::Id;
use tracing::error;

use crate::Cla;

#[async_trait]
impl cla::Cla for Cla {
    async fn register(&self) -> cla::Result<()> {
        // Announce every configured peer: each inbox directory is an
        // address this CLA can write to.
        for (eid, path) in &self.inboxes {
            self.sink
                .add_peer(
                    cla::Address::Private(Bytes::copy_from_slice(path.as_bytes())),
                    slice::from_ref(eid),
                )
                .await?;
        }

        // Watch the outbox, where configured
        if let Some(outbox) = &self.outbox {
            self.start_watcher(self.sink.clone(), outbox.clone(), self.max_bundle_size)
                .await;
        }

        Ok(())
    }

    async fn unregister(&self) -> cla::Result<()> {
        self.tasks.shutdown().await;
        Ok(())
    }

    // INTERIM BUFFERING: the bundle is written to the inbox file in one go,
    // so the stream is assembled in memory via `stream::buffer_stream`
    // before writing. This is a deliberate stepping stone toward the full
    // streaming pipeline (a native implementation would spool segments
    // straight to disk); see bpa/docs/streaming_pipeline_design.md.
    async fn forward(
        &self,
        _lane: Option<u32>,
        cla_addr: &cla::Address,
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardOutcome> {
        let bundle = buffer_stream(stream, total_len).await?;

        if let cla::Address::Private(remote_addr) = cla_addr
            && let Ok(addr_str) = str::from_utf8(remote_addr.as_ref())
            && self.inboxes.values().any(|p| p == addr_str)
        {
            // Write bundle to peer's inbox directory
            let mut filename = format!("{}_{}", bundle_id.source, bundle_id.timestamp)
                .replace(['\\', '/', ':', ' '], "_");
            if let Some(fragment_info) = &bundle_id.fragment_info {
                filename.push_str(format!("_fragment_{}", fragment_info.offset).as_str());
            }
            let path = PathBuf::from(addr_str).join(filename);

            return tokio::fs::write(&path, bundle)
                .await
                .map(|_| cla::ForwardOutcome::Sent)
                .map_err(|e| {
                    error!("Failed to write to '{}': {e}", path.display());
                    cla::Error::Internal(e.into())
                });
        }

        Ok(cla::ForwardOutcome::NoNeighbour)
    }
}
