use bytes::Bytes;
use hardy_async::async_trait;
#[cfg(feature = "instrument")]
use tracing::instrument;
use tracing::{debug, error};

use super::Bpa;
use crate::bundle;
use crate::cla;
use crate::egress::SendResult;
use crate::sink::Sink;

/// Wraps a CLA + peer info as a Sink for egress.
struct ClaSink<'a> {
    cla: &'a dyn cla::Cla,
    queue: Option<u32>,
    cla_addr: &'a cla::ClaAddress,
}

#[async_trait]
impl Sink for ClaSink<'_> {
    async fn write(&self, _bundle: &bundle::Bundle, data: Bytes) -> Result<(), crate::Error> {
        match self.cla.forward(self.queue, self.cla_addr, data).await {
            Ok(cla::ForwardBundleResult::Sent) => Ok(()),
            Ok(cla::ForwardBundleResult::NoNeighbour) => Err("Neighbour unavailable".into()),
            Err(e) => Err(e.into()),
        }
    }
}

impl Bpa {
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

        match self.egress.send(bundle, &sink).await {
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
