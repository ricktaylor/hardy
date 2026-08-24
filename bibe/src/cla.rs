use super::*;
use hardy_bpa::cla::{Cla, ClaAddress, ForwardBundleResult, Sink};

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
        let cbor_bytes = hardy_cbor::encode::emit(&decap_endpoint).0;
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
    pub(crate) async fn dispatch(&self, mut bundle: Bytes) -> Result<(), Error> {
        self.sink
            .get()
            .ok_or(Error::NotRegistered)?
            .dispatch(None, None, &mut bundle)
            .await?;
        Ok(())
    }

    /// Encapsulate an inner bundle into an outer bundle.
    pub fn encapsulate(&self, inner: Bytes, outer_dest: Eid) -> Result<Bytes, Error> {
        // Parse inner bundle to get lifetime for outer bundle
        let parsed = ParsedBundle::parse(&inner, bpsec::no_keys)?;
        let lifetime = parsed.bundle.lifetime;

        // Build outer bundle with BIBE-PDU payload:
        // [transmission-id, total-length, segmented-offset, encapsulated-bundle-segment]
        // For complete bundles: [0, 0, 0, bundle-bytes]
        let payload = hardy_cbor::encode::emit_array(Some(4), |a| {
            a.emit(&0u64); // transmission-id
            a.emit(&0u64); // total-length
            a.emit(&0u64); // segmented-offset

            // encapsulated-bundle-segment, as a definite-length byte string
            // (a bare `&[u8]` would encode as a CBOR array of integers, which
            // decapsulation rejects)
            a.emit(&hardy_cbor::encode::Bytes(inner.as_ref()));
        });

        let (_bundle, data) =
            hardy_bpv7::builder::Builder::new(self.tunnel_source.clone(), outer_dest)
                .with_lifetime(lifetime)
                .with_payload(Cow::Owned(payload))
                .build(CreationTimestamp::now())?;

        Ok(data.into())
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

    fn lane_count(&self) -> Option<core::num::NonZeroU32> {
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
        _bundle_id: &hardy_bpv7::bundle::Id,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
    ) -> hardy_bpa::cla::Result<ForwardBundleResult> {
        let bundle = hardy_bpa::stream::buffer_stream(stream, total_len).await?;

        // Decode destination EID from CBOR in ClaAddress
        let ClaAddress::Private(dest_bytes) = cla_addr else {
            warn!("BIBE forward called with non-Private ClaAddress");
            return Ok(ForwardBundleResult::NoNeighbour);
        };

        let outer_dest: Eid = match hardy_cbor::decode::parse(dest_bytes) {
            Ok(eid) => eid,
            Err(e) => {
                warn!("Failed to decode destination EID from ClaAddress: {e}");
                return Ok(ForwardBundleResult::NoNeighbour);
            }
        };

        debug!("BIBE encapsulating bundle to {outer_dest}");

        // Encapsulate the bundle
        let outer = match self.encapsulate(bundle, outer_dest) {
            Ok(outer) => outer,
            Err(e) => {
                warn!("BIBE encapsulation failed: {e}");
                return Ok(ForwardBundleResult::NoNeighbour);
            }
        };

        // Dispatch the outer bundle back into the BPA
        match self.dispatch(outer).await {
            Ok(()) => Ok(ForwardBundleResult::Sent),
            Err(e) => {
                warn!("BIBE dispatch failed: {e}");
                Ok(ForwardBundleResult::NoNeighbour)
            }
        }
    }
}
