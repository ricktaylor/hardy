use alloc::borrow::Cow;
use core::{
    num::{NonZeroU32, NonZeroU64},
    sync::atomic::{AtomicU64, Ordering},
};

use hardy_async::sync::spin::Once;
use hardy_bpa::{
    Bytes, async_trait,
    cla::{Acceptance, Cla, ClaAddress, ForwardBundleResult, Sink},
    stream::{Receiver, buffer_stream},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::Id,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, NodeId},
    parse::Parsed,
};
use hardy_cbor::encode::{emit, emit_array};
use tracing::{debug, warn};

use crate::Error;
/// BIBE CLA for encapsulation.
///
/// Implements `forward()` to encapsulate bundles and re-inject them into the BPA.
/// Virtual peers are registered via `add_tunnel()` with ClaAddress containing
/// the CBOR-encoded destination EID for the outer bundle.
pub struct BibeCla {
    tunnel_source: Eid,
    sink: Once<Box<dyn Sink>>,
    // The BPA's dispatch size cap, learned at registration (0 = not yet
    // registered): an encapsulated outer bundle over the cap would be
    // refused deterministically, so it is never dispatched.
    max_bundle_size: AtomicU64,
}

impl BibeCla {
    /// Create a new BibeCla with the given tunnel source EID.
    pub fn new(tunnel_source: Eid) -> Self {
        Self {
            tunnel_source,
            sink: Once::new(),
            max_bundle_size: AtomicU64::new(0),
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
        let cbor_bytes = emit(&decap_endpoint).0;
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

    /// Encapsulate an inner bundle into an outer bundle.
    fn encapsulate(&self, inner: Bytes, outer_dest: Eid) -> Result<Bytes, Error> {
        // Parse inner bundle structurally to read its lifetime.
        let Parsed {
            data: inner,
            bundle: parsed_bundle,
            ..
        } = hardy_bpv7::parse::parse(inner)?;
        let lifetime = parsed_bundle.primary.lifetime;

        // Build outer bundle with BIBE-PDU payload:
        // [transmission-id, total-length, segmented-offset, encapsulated-bundle-segment]
        // For complete bundles: [0, 0, 0, bundle-bytes]
        let payload = emit_array(Some(4), |a| {
            a.emit(&0u64); // transmission-id
            a.emit(&0u64); // total-length
            a.emit(&0u64); // segmented-offset
            a.emit(inner.as_ref()); // encapsulated-bundle-segment
        });

        let (_bundle, data) = Builder::new(self.tunnel_source.clone(), outer_dest)
            .with_lifetime(lifetime)
            .with_payload(Cow::Owned(payload))
            .build(CreationTimestamp::now())?;

        Ok(data.into())
    }
}

#[async_trait]
impl Cla for BibeCla {
    async fn on_register(
        &self,
        sink: Box<dyn Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: NonZeroU64,
    ) {
        self.sink.call_once(|| sink);
        self.max_bundle_size
            .store(max_bundle_size.get(), Ordering::Relaxed);
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
        stream: &mut dyn Receiver<hardy_bpa::cla::Segment>,
    ) -> hardy_bpa::cla::Result<ForwardBundleResult> {
        let bundle = buffer_stream(stream, total_len).await?;

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

        // Pre-check against the BPA's dispatch size cap: encapsulation grows
        // the bundle, and an over-cap outer would be refused
        // deterministically on every retry. (cap == 0 only before
        // registration, when no forward can arrive.)
        let cap = self.max_bundle_size.load(Ordering::Relaxed);
        if cap > 0 && outer.len() as u64 > cap {
            warn!(
                "BIBE outer bundle exceeds the BPA's max bundle size ({} > {cap})",
                outer.len()
            );
            return Ok(ForwardBundleResult::NoNeighbour);
        }

        // Dispatch the outer bundle back into the BPA
        match self.dispatch(outer).await {
            Ok(Acceptance::Accepted) => Ok(ForwardBundleResult::Sent),
            Ok(Acceptance::Refused) => {
                warn!("BIBE outer bundle refused by the BPA");
                Ok(ForwardBundleResult::NoNeighbour)
            }
            Err(e) => {
                warn!("BIBE dispatch failed: {e}");
                Ok(ForwardBundleResult::NoNeighbour)
            }
        }
    }
}
