use alloc::boxed::Box;
use core::num::NonZeroU32;

use hardy_async::sync::spin::Once;
use hardy_bpa::{
    Bytes, async_trait,
    cla::{
        Acceptance, Cla, ClaAddress, Error as ClaError, ForwardBundleResult, Result as ClaResult,
        Segment, Sink,
    },
    stream::{Receiver, buffer_stream},
};
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, NodeId},
};
use hardy_cbor::{decode, encode};
use tracing::{debug, warn};

use crate::{Error, pdu};

/// BIBE CLA for encapsulation.
///
/// Implements `forward()` to encapsulate bundles and re-inject them into the BPA.
/// Virtual peers are registered via `add_tunnel()` with ClaAddress containing
/// the CBOR-encoded destination EID for the outer bundle.
pub struct BibeCla {
    tunnel_source: Eid,
    sink: Once<Box<dyn Sink>>,
}

impl BibeCla {
    /// Create a new BibeCla with the given tunnel source EID.
    pub fn new(tunnel_source: Eid) -> Self {
        Self {
            tunnel_source,
            sink: Once::new(),
        }
    }

    /// Unregister this CLA from the BPA.
    pub async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }

    /// Register a tunnel destination as a virtual peer.
    ///
    /// The `tunnel_id` NodeId becomes routable, and bundles forwarded to it
    /// will be encapsulated with `decap_endpoint` as the outer destination.
    pub async fn add_tunnel(&self, tunnel_id: NodeId, decap_endpoint: Eid) -> Result<(), Error> {
        // Encode the decap endpoint as CBOR
        let cbor_bytes = encode::emit(&decap_endpoint).0;
        let cla_addr = ClaAddress::Private(cbor_bytes.into());

        // Register as a peer - this creates the local route entry
        self.sink
            .get()
            .ok_or(Error::NotRegistered)?
            .add_peer(cla_addr, &[tunnel_id])
            .await?;

        Ok(())
    }

    /// Dispatch a bundle into the BPA (used by DecapService).
    // INTERIM BUFFERING: both callers (decapsulation and encapsulation) hold
    // a complete bundle in memory, so it enters the BPA as a one-segment
    // stream (`Bytes` is a `stream::Receiver`). This is a deliberate stepping stone toward
    // the full streaming pipeline; see bpa/docs/streaming_pipeline_design.md.
    pub(crate) async fn dispatch(&self, mut bundle: Bytes) -> Result<Acceptance, Error> {
        Ok(self
            .sink
            .get()
            .ok_or(Error::NotRegistered)?
            .dispatch(None, None, &mut bundle)
            .await?)
    }
}

#[async_trait]
impl Cla for BibeCla {
    async fn on_register(&self, sink: Box<dyn Sink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
        debug!("BIBE CLA registered");
    }

    async fn on_unregister(&self) {
        debug!("BIBE CLA unregistered");
    }

    fn lane_count(&self) -> Option<NonZeroU32> {
        None
    }

    // INTERIM BUFFERING: encapsulation wraps the whole inner bundle in a
    // single BIBE-PDU byte string with a whole-buffer codec, so the stream
    // is assembled in memory via `stream::buffer_stream` first. This is a
    // deliberate stepping stone toward the full streaming pipeline; see
    // bpa/docs/streaming_pipeline_design.md.
    async fn forward(
        &self,
        _lane: Option<u32>,
        cla_addr: &ClaAddress,
        _bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> ClaResult<ForwardBundleResult> {
        let bundle = buffer_stream(stream, total_len).await?;

        // Decode destination EID from CBOR in ClaAddress
        let ClaAddress::Private(dest_bytes) = cla_addr else {
            warn!("BIBE forward called with non-Private ClaAddress");
            return Ok(ForwardBundleResult::NoNeighbour);
        };

        let outer_dest: Eid = match decode::parse(dest_bytes) {
            Ok(eid) => eid,
            Err(e) => {
                warn!("Failed to decode destination EID from ClaAddress: {e}");
                return Ok(ForwardBundleResult::NoNeighbour);
            }
        };

        debug!("BIBE encapsulating bundle to {outer_dest}");

        // Encapsulate the bundle
        let outer = match pdu::encapsulate(&self.tunnel_source, bundle, outer_dest) {
            Ok(outer) => outer,
            Err(e) => {
                warn!("BIBE encapsulation failed: {e}");
                return Ok(ForwardBundleResult::NoNeighbour);
            }
        };

        // Dispatch the outer bundle back into the BPA
        match self.dispatch(outer).await {
            Ok(Acceptance::Accepted) => Ok(ForwardBundleResult::Sent),
            // The BPA refused this bundle: a per-bundle verdict, reported as
            // such so the dispatcher parks this bundle alone.
            Ok(Acceptance::Refused) => Err(ClaError::Internal(Box::new(Error::Refused))),
            // The sink is genuinely gone — the one link-scoped outcome.
            Err(Error::Dispatch(ClaError::Disconnected)) => Ok(ForwardBundleResult::NoNeighbour),
            Err(Error::Dispatch(e)) => Err(e),
            Err(e) => Err(ClaError::Internal(Box::new(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use hardy_bpa::cla::TransferOutcome;
    use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp};

    use super::*;

    struct StubSink {
        verdict: Acceptance,
    }

    #[async_trait]
    impl Sink for StubSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&ClaAddress>,
            stream: &mut dyn Receiver<Segment>,
        ) -> ClaResult<Acceptance> {
            // Drain the one-segment stream like the real door would.
            while let Ok(segment) = stream.recv().await {
                if matches!(segment, Segment::Final(_)) {
                    break;
                }
            }
            Ok(self.verdict)
        }

        async fn add_peer(&self, _cla_addr: ClaAddress, _node_ids: &[NodeId]) -> ClaResult<bool> {
            Ok(true)
        }

        async fn remove_peer(&self, _cla_addr: &ClaAddress) -> ClaResult<bool> {
            Ok(true)
        }

        async fn transfer_outcome(
            &self,
            _bundle_id: &Id,
            _outcome: TransferOutcome,
        ) -> ClaResult<()> {
            Ok(())
        }
    }

    fn inner_bundle() -> (Id, Bytes) {
        let (bundle, data) = Builder::new("ipn:10.1".parse().unwrap(), "ipn:20.1".parse().unwrap())
            .with_payload(vec![0u8; 256].into())
            .build(CreationTimestamp::now())
            .unwrap();
        (bundle.primary.id, Bytes::from(data))
    }

    fn tunnel_addr() -> ClaAddress {
        let outer_dest: Eid = "ipn:30.1".parse().unwrap();
        ClaAddress::Private(Bytes::from(encode::emit(&outer_dest).0))
    }

    async fn registered_cla(verdict: Acceptance) -> BibeCla {
        let cla = BibeCla::new("ipn:10.0".parse().unwrap());
        cla.on_register(Box::new(StubSink { verdict }), &[]).await;
        cla
    }

    // A BPA refusal of the outer bundle is this bundle's verdict, surfaced
    // as an error the dispatcher parks per-bundle.
    #[tokio::test]
    async fn bpa_refusal_is_not_no_neighbour() {
        let cla = registered_cla(Acceptance::Refused).await;

        let (id, mut inner) = inner_bundle();
        let total_len = inner.len() as u64;
        let result = cla
            .forward(None, &tunnel_addr(), &id, total_len, &mut inner)
            .await;

        assert!(matches!(result, Err(ClaError::Internal(_))));
    }
}
