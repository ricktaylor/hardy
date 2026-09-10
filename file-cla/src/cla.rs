use core::num::NonZeroU32;
use std::{path::PathBuf, slice, sync::Arc};

use hardy_bpa::{
    async_trait,
    stream::{Receiver, buffer_stream},
};
use hardy_bpv7::{bundle::Id, eid::NodeId};
use tracing::{error, warn};

use crate::Cla;
#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::cla::Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: core::num::NonZeroU64,
    ) {
        // Register all peers with the BPA
        for (eid, path) in &self.inboxes {
            if let Err(e) = sink
                .add_peer(
                    hardy_bpa::cla::ClaAddress::Private(hardy_bpa::Bytes::copy_from_slice(
                        path.as_bytes(),
                    )),
                    slice::from_ref(eid),
                )
                .await
            {
                warn!("add_peer() failed: {e}");
                return;
            }
        }

        let sink: Arc<dyn hardy_bpa::cla::Sink> = sink.into();
        let sink = self.sink.call_once(|| sink);

        // Start the file watcher if outbox is configured
        if let Some(outbox) = &self.outbox {
            self.start_watcher(sink.clone(), outbox.clone(), max_bundle_size)
                .await;
        }
    }

    async fn on_unregister(&self) {
        self.tasks.shutdown().await;
    }

    fn lane_count(&self) -> Option<NonZeroU32> {
        None
    }

    // INTERIM BUFFERING: the bundle is written to the inbox file in one go,
    // so the stream is assembled in memory via `stream::buffer_stream`
    // before writing. This is a deliberate stepping stone toward the full
    // streaming pipeline (a native implementation would spool segments
    // straight to disk); see bpa/docs/streaming_pipeline_design.md.
    async fn forward(
        &self,
        _lane: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<hardy_bpa::cla::Segment>,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        let _sink = self.sink.get().ok_or_else(|| {
            error!("forward called before on_register!");
            hardy_bpa::cla::Error::Disconnected
        })?;

        let bundle = buffer_stream(stream, total_len).await?;

        if let hardy_bpa::cla::ClaAddress::Private(remote_addr) = cla_addr
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
                .map(|_| hardy_bpa::cla::ForwardBundleResult::Sent)
                .map_err(|e| {
                    error!("Failed to write to '{}': {e}", path.display());
                    hardy_bpa::cla::Error::Internal(e.into())
                });
        }

        Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour)
    }
}
