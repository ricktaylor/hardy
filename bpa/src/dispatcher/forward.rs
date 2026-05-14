use super::*;
use crate::egress::{Egress, SendResult};
use crate::sink::Sink;

/// Wraps a CLA + peer info as a Sink for egress.
struct ClaSink<'a> {
    cla: &'a dyn cla::Cla,
    queue: Option<u32>,
    cla_addr: &'a cla::ClaAddress,
}

#[async_trait]
impl Sink for ClaSink<'_> {
    async fn write(&self, data: Bytes) -> Result<(), crate::Error> {
        match self.cla.forward(self.queue, self.cla_addr, data).await {
            Ok(cla::ForwardBundleResult::Sent) => Ok(()),
            Ok(cla::ForwardBundleResult::NoNeighbour) => Err("Neighbour unavailable".into()),
            Err(e) => Err(e.into()),
        }
    }
}

impl Dispatcher {
    pub(crate) fn egress(&self) -> Egress<'_> {
        Egress {
            store: self.store.clone(),
            key_store: self.key_store.clone(),
            filter_engine: self.filter_engine.clone(),
            processing_pool: &self.processing_pool,
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, cla, bundle), fields(bundle.id = %bundle.bundle.id)))]
    pub async fn forward_bundle(
        &self,
        cla: &dyn cla::Cla,
        peer: u32,
        queue: Option<u32>,
        cla_addr: &cla::ClaAddress,
        bundle: bundle::Bundle,
    ) {
        let sink = ClaSink {
            cla,
            queue,
            cla_addr,
        };

        match self.egress().send(bundle, &sink).await {
            Ok(SendResult::Sent) => {
                // TODO: reporting
            }
            Ok(SendResult::Rejected) => {
                debug!("CLA rejected bundle for peer {peer}, resetting queue");
                self.store.reset_peer_queue(peer).await;
            }
            Ok(SendResult::Filtered) | Ok(SendResult::NotFound) => {}
            Err(e) => {
                error!("Egress processing failed for peer {peer}: {e}");
            }
        }
    }
}
