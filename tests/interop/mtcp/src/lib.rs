mod codec;
pub mod config;
mod connect;
mod listen;

use hardy_async::sync::spin::Once;
use hardy_bpv7::eid::NodeId;
use std::sync::Arc;
use trace_err::*;
use tracing::{debug, error, info, warn};

/// Registration-time state from BPA.
struct Inner {
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    #[allow(dead_code)]
    node_ids: Arc<[NodeId]>,
}

pub struct Cla {
    config: config::Config,
    inner: Once<Inner>,
    tasks: Arc<hardy_async::TaskPool>,
}

impl Cla {
    pub fn new(config: config::Config) -> Self {
        Self {
            config,
            inner: Once::new(),
            tasks: Arc::new(hardy_async::TaskPool::new()),
        }
    }
}

#[hardy_bpa::async_trait]
impl hardy_bpa::cla::Cla for Cla {
    async fn on_register(&self, sink: Box<dyn hardy_bpa::cla::Sink>, node_ids: &[NodeId]) {
        let sink: Arc<dyn hardy_bpa::cla::Sink> = sink.into();

        self.inner.call_once(|| Inner {
            sink: sink.clone(),
            node_ids: node_ids.into(),
        });

        // Register static peer if configured
        if let (Some(peer_addr), Some(peer_node)) =
            (&self.config.peer, &self.config.peer_node)
        {
            if let Ok(addr) = peer_addr.parse::<std::net::SocketAddr>() {
                if let Ok(node_id) = peer_node.parse::<NodeId>() {
                    let cla_addr = hardy_bpa::cla::ClaAddress::Tcp(addr);
                    match sink.add_peer(cla_addr, &[node_id]).await {
                        Ok(true) => info!("Registered peer {peer_node} at {peer_addr}"),
                        Ok(false) => warn!("Peer {peer_node} at {peer_addr} already registered"),
                        Err(e) => error!("Failed to register peer: {e:?}"),
                    }
                } else {
                    error!("Invalid peer-node EID: {peer_node}");
                }
            } else {
                error!("Invalid peer address: {peer_addr}");
            }
        }

        // Start listener if address is configured
        if let Some(address) = self.config.address {
            let listener = listen::Listener {
                address,
                framing: self.config.framing.clone(),
                max_bundle_size: self.config.max_bundle_size,
                sink,
            };
            let tasks = self.tasks.clone();
            hardy_async::spawn!(self.tasks, "mtcp_listener", async move {
                listener.listen(tasks).await;
            });
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
        let hardy_bpa::cla::ClaAddress::Tcp(remote_addr) = cla_addr else {
            return Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour);
        };

        debug!("Forwarding bundle ({} bytes) to {remote_addr}", bundle.len());

        connect::forward(remote_addr, &self.config.framing, bundle)
            .await
            .map_err(|e| {
                debug!("Forward failed: {e}");
                hardy_bpa::cla::Error::Internal(e.into())
            })?;

        Ok(hardy_bpa::cla::ForwardBundleResult::Sent)
    }
}

// --- Plugin entry point ---

hardy_plugin_abi::export_cla!(config::Config, |config| {
    Ok(Arc::new(Cla::new(config)))
});
