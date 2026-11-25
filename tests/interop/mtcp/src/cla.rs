use core::num::NonZeroU64;

use hardy_bpa::{
    async_trait,
    cla::{
        self, Address, Error as ClaError, Factory, ForwardOutcome, Result as ClaResult, Segment,
        Sink, factory_fn,
    },
    stream::{Receiver, buffer_stream},
};
use hardy_bpv7::bundle::Id;

use super::*;

pub struct Cla {
    config: config::ClaConfig,
    sink: Arc<dyn Sink>,
    node_ids: Arc<NodeIds>,
    // The inbound framing bound: the configured limit folded with the
    // cap the BPA settled (0 = unbounded). A frame the BPA would only
    // reject deterministically is refused at the codec instead of being
    // buffered whole first.
    max_bundle_size: u64,
    tasks: Arc<TaskPool>,
}

impl Cla {
    /// A factory for an MTCP CLA configured by `config`.
    pub fn factory(config: config::ClaConfig) -> impl Factory<Component = Self> {
        // The configured inbound framing bound doubles as the declared
        // receive limit (0 = unbounded, declared as no limit).
        let request = Request {
            address_type: Some(AddressType::Tcp),
            max_bundle_size: NonZeroU64::new(config.max_bundle_size),
            ..Default::default()
        };

        factory_fn(request, move |cx: Context| {
            let max_bundle_size = match (config.max_bundle_size, cx.terms.max_bundle_size) {
                (0, Some(cap)) => cap.get(),
                (configured, Some(cap)) => configured.min(cap.get()),
                (configured, None) => configured,
            };
            Ok(Arc::new(Self {
                config,
                sink: cx.sink,
                node_ids: cx.node_ids,
                max_bundle_size,
                tasks: Arc::new(TaskPool::new()),
            }))
        })
    }

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        self.tasks.shutdown().await;
        self.sink.unregister().await;
    }
}

#[async_trait]
impl cla::Cla for Cla {
    async fn register(&self) -> ClaResult<()> {
        info!("CLA starting, node IDs: {:?}", self.node_ids);

        if let (Some(peer_addr), Some(peer_node)) = (&self.config.peer, &self.config.peer_node) {
            if let Ok(addr) = peer_addr.parse::<std::net::SocketAddr>() {
                if let Ok(node_id) = peer_node.parse::<NodeId>() {
                    let cla_addr = Address::Tcp(addr);
                    match self.sink.add_peer(cla_addr, &[node_id]).await {
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

        if let Some(address) = self.config.address {
            let listener = listen::Listener {
                address,
                framing: self.config.framing.clone(),
                max_bundle_size: self.max_bundle_size,
                sink: self.sink.clone(),
            };
            let tasks = self.tasks.clone();
            hardy_async::spawn!(self.tasks, "mtcp_listener", async move {
                listener.listen(tasks).await;
            });
        }

        Ok(())
    }

    async fn unregister(&self) -> ClaResult<()> {
        self.tasks.shutdown().await;
        Ok(())
    }

    // MTCP frames the whole bundle as one CBOR byte string, so the stream
    // is assembled in memory with `stream::buffer_stream` before framing.
    // See bpa/docs/streaming_pipeline_design.md for the chunked form this
    // is a step towards.
    async fn forward(
        &self,
        _lane: Option<u32>,
        cla_addr: &Address,
        _bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> ClaResult<ForwardOutcome> {
        let Address::Tcp(remote_addr) = cla_addr else {
            return Ok(ForwardOutcome::NoNeighbour);
        };

        let bundle = buffer_stream(stream, total_len).await?;

        debug!(
            "Forwarding bundle ({} bytes) to {remote_addr}",
            bundle.len()
        );

        connect::forward(remote_addr, &self.config.framing, bundle)
            .await
            .map_err(|e| {
                debug!("Forward failed: {e}");
                ClaError::Internal(e.into())
            })?;

        Ok(ForwardOutcome::Sent)
    }
}
