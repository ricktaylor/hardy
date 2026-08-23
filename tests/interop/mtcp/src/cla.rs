use super::*;

pub struct Cla {
    config: config::ClaConfig,
    sink: Once<Arc<dyn hardy_bpa::cla::Sink>>,
    tasks: Arc<hardy_async::TaskPool>,
}

impl Cla {
    pub fn new(config: config::ClaConfig) -> Self {
        Self {
            config,
            sink: Once::new(),
            tasks: Arc::new(hardy_async::TaskPool::new()),
        }
    }

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        self.tasks.shutdown().await;
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }
}

#[hardy_bpa::async_trait]
impl hardy_bpa::cla::Cla for Cla {
    fn address_type(&self) -> Option<hardy_bpa::cla::ClaAddressType> {
        Some(hardy_bpa::cla::ClaAddressType::Tcp)
    }

    async fn on_register(&self, sink: Box<dyn hardy_bpa::cla::Sink>, _node_ids: &[NodeId]) {
        let sink: Arc<dyn hardy_bpa::cla::Sink> = sink.into();
        self.sink.call_once(|| sink.clone());

        // Register static peer if configured
        if let (Some(peer_addr), Some(peer_node)) = (&self.config.peer, &self.config.peer_node) {
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

    fn lane_count(&self) -> Option<core::num::NonZeroU32> {
        None
    }

    // INTERIM BUFFERING: MTCP frames the whole bundle as one CBOR byte
    // string, so the stream is assembled in memory via
    // `stream::buffer_stream` before framing. This is a deliberate stepping
    // stone toward the full streaming pipeline; see
    // bpa/docs/streaming_pipeline_design.md.
    async fn forward(
        &self,
        _lane: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        let hardy_bpa::cla::ClaAddress::Tcp(remote_addr) = cla_addr else {
            return Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour);
        };

        let bundle = hardy_bpa::stream::buffer_stream(stream, total_len).await?;

        debug!(
            "Forwarding bundle ({} bytes) to {remote_addr}",
            bundle.len()
        );

        connect::forward(remote_addr, &self.config.framing, bundle)
            .await
            .map_err(|e| {
                debug!("Forward failed: {e}");
                hardy_bpa::cla::Error::Internal(e.into())
            })?;

        Ok(hardy_bpa::cla::ForwardBundleResult::Sent)
    }
}
