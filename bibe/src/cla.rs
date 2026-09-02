use alloc::boxed::Box;
use core::num::NonZeroU64;

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

// The registration state, set exactly once: the sink and the negotiated
// dispatch size cap are only ever obtainable together, so no pre-check can
// run against a cap from a registration that has not happened.
struct Inner {
    sink: Box<dyn Sink>,
    // The negotiated dispatch size cap, if any: an encapsulated outer
    // bundle over the cap would be rejected deterministically, so it is
    // never dispatched.
    max_bundle_size: Option<NonZeroU64>,
}

/// BIBE CLA for encapsulation.
///
/// Implements `forward()` to encapsulate bundles and re-inject them into the BPA.
/// Virtual peers are registered via `add_tunnel()` with ClaAddress containing
/// the CBOR-encoded destination EID for the outer bundle.
pub struct BibeCla {
    tunnel_source: Eid,
    inner: Once<Inner>,
}

impl BibeCla {
    /// Create a new BibeCla with the given tunnel source EID.
    pub fn new(tunnel_source: Eid) -> Self {
        Self {
            tunnel_source,
            inner: Once::new(),
        }
    }

    /// Unregister this CLA from the BPA.
    pub async fn unregister(&self) {
        if let Some(inner) = self.inner.get() {
            inner.sink.unregister().await;
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
        self.inner
            .get()
            .ok_or(Error::NotRegistered)?
            .sink
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
            .inner
            .get()
            .ok_or(Error::NotRegistered)?
            .sink
            .dispatch(None, None, &mut bundle)
            .await?)
    }
}

#[async_trait]
impl Cla for BibeCla {
    async fn on_register(
        &self,
        sink: Box<dyn Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        self.inner.call_once(|| Inner {
            sink,
            max_bundle_size,
        });
        debug!("BIBE CLA registered");
    }

    async fn on_unregister(&self) {
        debug!("BIBE CLA unregistered");
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

        // Pre-check against the negotiated dispatch size cap: encapsulation
        // grows the bundle, and an over-cap outer would be rejected
        // deterministically on every retry. A per-bundle condition, never
        // `NoNeighbour`: the link-scoped signal would sweep the whole peer
        // queue back to Waiting on every routing event for this bundle's
        // lifetime. (A forward can only arrive after registration, so the
        // cap read here is the registration's.)
        if let Some(inner) = self.inner.get()
            && let Some(cap) = inner.max_bundle_size
            && outer.len() as u64 > cap.get()
        {
            return Err(ClaError::PayloadTooLarge {
                size: outer.len() as u64,
                max: cap.get(),
            });
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

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, sync::Arc};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use hardy_bpa::cla::TransferOutcome;
    use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp};

    use super::*;

    /// Counts the bundles the CLA dispatches back into the BPA.
    struct MockSink {
        dispatched: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&ClaAddress>,
            stream: &mut dyn Receiver<Segment>,
        ) -> ClaResult<()> {
            // Drain the stream to completion, as a real dispatcher would.
            while let Ok(segment) = stream.recv().await {
                if matches!(segment, Segment::Final(_)) {
                    break;
                }
            }
            self.dispatched.fetch_add(1, Ordering::Relaxed);
            Ok(())
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
        let (bundle, data) = Builder::new("ipn:2.1".parse().unwrap(), "ipn:3.1".parse().unwrap())
            .with_payload(Cow::Borrowed(b"BIBE cap test payload".as_slice()))
            .build(CreationTimestamp::now())
            .expect("Failed to build inner bundle");
        (bundle.primary.id, Bytes::from(data))
    }

    /// A registered BIBE CLA with the given negotiated cap, plus the
    /// dispatch counter its sink feeds.
    async fn registered_cla(cap: Option<NonZeroU64>) -> (BibeCla, Arc<AtomicUsize>) {
        let cla = BibeCla::new("ipn:1.0".parse().unwrap());
        let dispatched = Arc::new(AtomicUsize::new(0));
        cla.on_register(
            Box::new(MockSink {
                dispatched: dispatched.clone(),
            }),
            &[],
            cap,
        )
        .await;
        (cla, dispatched)
    }

    fn tunnel_addr() -> ClaAddress {
        let decap_endpoint: Eid = "ipn:4.1".parse().unwrap();
        ClaAddress::Private(encode::emit(&decap_endpoint).0.into())
    }

    /// Encapsulation grows the bundle, so a cap equal to the inner bundle's
    /// size is always exceeded by the outer: forward refuses per-bundle
    /// with `PayloadTooLarge` carrying the negotiated cap, and nothing is
    /// dispatched.
    #[tokio::test]
    async fn over_cap_outer_is_refused_with_payload_too_large() {
        let (bundle_id, inner) = inner_bundle();
        let cap = NonZeroU64::new(inner.len() as u64).unwrap();
        let (cla, dispatched) = registered_cla(Some(cap)).await;

        let total_len = inner.len() as u64;
        let mut stream = inner;
        let result = cla
            .forward(None, &tunnel_addr(), &bundle_id, total_len, &mut stream)
            .await;

        let Err(ClaError::PayloadTooLarge { size, max }) = result else {
            panic!("Expected PayloadTooLarge");
        };
        assert_eq!(max, cap.get());
        assert!(
            size > max,
            "The outer bundle must exceed the cap it tripped"
        );
        assert_eq!(dispatched.load(Ordering::Relaxed), 0);
    }

    /// An outer bundle within the negotiated cap is dispatched back into
    /// the BPA and reported `Sent`.
    #[tokio::test]
    async fn under_cap_outer_is_dispatched() {
        let (bundle_id, inner) = inner_bundle();
        // Generous headroom for the encapsulation overhead.
        let cap = NonZeroU64::new(inner.len() as u64 + 1024).unwrap();
        let (cla, dispatched) = registered_cla(Some(cap)).await;

        let total_len = inner.len() as u64;
        let mut stream = inner;
        let result = cla
            .forward(None, &tunnel_addr(), &bundle_id, total_len, &mut stream)
            .await;

        assert!(matches!(result, Ok(ForwardBundleResult::Sent)));
        assert_eq!(dispatched.load(Ordering::Relaxed), 1);
    }
}
