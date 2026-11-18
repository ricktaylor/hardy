use alloc::{boxed::Box, sync::Arc};
use core::num::NonZeroU64;

use hardy_bpa::{
    Bytes, async_trait,
    cla::{
        Acceptance, Address, AddressType, Cla, Context, Error as ClaError, Factory, ForwardOutcome,
        Request, Result as ClaResult, Segment, Sink, factory_fn,
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
/// Virtual peers are registered via `add_tunnel()` with an
/// `Address::Private` carrying the CBOR-encoded destination EID of the
/// outer bundle.
pub struct BibeCla {
    tunnel_source: Eid,
    sink: Arc<dyn Sink>,
    // The settled receive limit, if any: an encapsulated outer bundle
    // over the limit would be refused deterministically, so it is never
    // dispatched.
    max_reception_size: Option<NonZeroU64>,
}

impl BibeCla {
    /// A factory for a BIBE CLA whose outer bundles are sourced at
    /// `tunnel_source`.
    pub fn factory(tunnel_source: Eid) -> impl Factory<Component = Self> {
        // Tunnel destinations are `Address::Private`, and no receive limit
        // is declared: encapsulation overhead is per bundle, so the outer
        // bundle is measured against the settled limit per forward.
        let request = Request {
            address_type: Some(AddressType::Private),
            ..Default::default()
        };

        factory_fn(request, move |cx: Context| {
            Ok(Arc::new(Self {
                tunnel_source,
                sink: cx.sink,
                max_reception_size: cx.terms.max_reception_size,
            }))
        })
    }

    /// Unregister this CLA from the BPA.
    pub async fn unregister(&self) {
        self.sink.unregister().await;
    }

    /// Register a tunnel destination as a virtual peer.
    ///
    /// The `tunnel_id` NodeId becomes routable, and bundles forwarded to it
    /// will be encapsulated with `decap_endpoint` as the outer destination.
    pub async fn add_tunnel(&self, tunnel_id: NodeId, decap_endpoint: Eid) -> Result<(), Error> {
        // Encode the decap endpoint as CBOR
        let cbor_bytes = encode::emit(&decap_endpoint).0;
        let cla_addr = Address::Private(cbor_bytes.into());

        // Register as a peer - this creates the local route entry
        self.sink.add_peer(cla_addr, &[tunnel_id]).await?;

        Ok(())
    }

    /// Dispatch a bundle into the BPA (used by DecapService).
    // INTERIM BUFFERING: both callers (decapsulation and encapsulation) hold
    // a complete bundle in memory, so it enters the BPA as a one-segment
    // stream (`Bytes` is a `stream::Receiver`). This is a deliberate stepping stone toward
    // the full streaming pipeline; see bpa/docs/streaming_pipeline_design.md.
    pub(crate) async fn dispatch(&self, mut bundle: Bytes) -> Result<Acceptance, Error> {
        Ok(self.sink.dispatch(None, None, &mut bundle).await?)
    }
}

#[async_trait]
impl Cla for BibeCla {
    // INTERIM BUFFERING: encapsulation wraps the whole inner bundle in a
    // single BIBE-PDU byte string with a whole-buffer codec, so the stream
    // is assembled in memory via `stream::buffer_stream` first. This is a
    // deliberate stepping stone toward the full streaming pipeline; see
    // bpa/docs/streaming_pipeline_design.md.
    async fn forward(
        &self,
        _lane: Option<u32>,
        cla_addr: &Address,
        _bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> ClaResult<ForwardOutcome> {
        let bundle = buffer_stream(stream, total_len).await?;

        // The outer bundle's destination is CBOR inside the peer address.
        let Address::Private(dest_bytes) = cla_addr else {
            warn!("BIBE forward called with a non-Private address");
            return Ok(ForwardOutcome::NoNeighbour);
        };

        let outer_dest: Eid = match decode::parse(dest_bytes) {
            Ok(eid) => eid,
            Err(e) => {
                warn!("Failed to decode destination EID from Address: {e}");
                return Ok(ForwardOutcome::NoNeighbour);
            }
        };

        debug!("BIBE encapsulating bundle to {outer_dest}");

        // Encapsulate the bundle
        let outer = match pdu::encapsulate(&self.tunnel_source, bundle, outer_dest) {
            Ok(outer) => outer,
            Err(e) => {
                warn!("BIBE encapsulation failed: {e}");
                return Ok(ForwardOutcome::NoNeighbour);
            }
        };

        // Pre-check against the settled receive limit: encapsulation grows
        // the bundle, and an over-limit outer would be rejected
        // deterministically on every retry. A per-bundle condition, never
        // `NoNeighbour`: the link-scoped signal would sweep the whole peer
        // queue back to Waiting on every routing event for this bundle's
        // lifetime.
        if let Some(cap) = self.max_reception_size
            && outer.len() as u64 > cap.get()
        {
            return Err(ClaError::PayloadTooLarge {
                size: outer.len() as u64,
                max: cap.get(),
            });
        }

        // Dispatch the outer bundle back into the BPA
        match self.dispatch(outer).await {
            Ok(Acceptance::Accepted) => Ok(ForwardOutcome::Sent),
            // The BPA refused this bundle: a per-bundle verdict, reported as
            // such so the dispatcher parks this bundle alone.
            Ok(Acceptance::Refused) => Err(ClaError::Internal(Box::new(Error::Refused))),
            // The sink is genuinely gone — the one link-scoped outcome.
            Err(Error::Dispatch(ClaError::Disconnected)) => Ok(ForwardOutcome::NoNeighbour),
            Err(Error::Dispatch(e)) => Err(e),
            Err(e) => Err(ClaError::Internal(Box::new(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, sync::Arc};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use hardy_bpa::{
        cla::{Terms, TransferOutcome},
        node_ids::NodeIds,
    };
    use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp};

    use super::*;

    /// Counts the bundles the CLA dispatches back into the BPA, answering
    /// each with a configured verdict.
    struct MockSink {
        verdict: Acceptance,
        dispatched: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&Address>,
            stream: &mut dyn Receiver<Segment>,
        ) -> ClaResult<Acceptance> {
            // Drain the stream to completion, as a real dispatcher would.
            while let Ok(segment) = stream.recv().await {
                if matches!(segment, Segment::Final(_)) {
                    break;
                }
            }
            self.dispatched.fetch_add(1, Ordering::Relaxed);
            Ok(self.verdict)
        }

        async fn add_peer(&self, _cla_addr: Address, _node_ids: &[NodeId]) -> ClaResult<bool> {
            Ok(true)
        }

        async fn remove_peer(&self, _cla_addr: &Address) -> ClaResult<bool> {
            Ok(true)
        }

        async fn report_transfer(
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

    /// A BIBE CLA built under the given settled receive limit, on a sink
    /// answering every dispatch with `verdict`, plus the dispatch counter
    /// that sink feeds.
    fn registered_cla(
        cap: Option<NonZeroU64>,
        verdict: Acceptance,
    ) -> (Arc<BibeCla>, Arc<AtomicUsize>) {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let terms = Terms {
            max_reception_size: cap,
            ..Default::default()
        };

        let cla = BibeCla::factory("ipn:1.0".parse().unwrap())
            .build(Context::new(
                Arc::new(MockSink {
                    verdict,
                    dispatched: dispatched.clone(),
                }),
                Arc::new(NodeIds::default()),
                terms,
            ))
            .expect("a BIBE CLA builds under any terms");
        (cla, dispatched)
    }

    fn tunnel_addr() -> Address {
        let decap_endpoint: Eid = "ipn:4.1".parse().unwrap();
        Address::Private(encode::emit(&decap_endpoint).0.into())
    }

    /// Encapsulation grows the bundle, so a cap equal to the inner bundle's
    /// size is always exceeded by the outer: forward refuses per-bundle
    /// with `PayloadTooLarge` carrying the negotiated cap, and nothing is
    /// dispatched.
    #[tokio::test]
    async fn over_cap_outer_is_refused_with_payload_too_large() {
        let (bundle_id, inner) = inner_bundle();
        let cap = NonZeroU64::new(inner.len() as u64).unwrap();
        let (cla, dispatched) = registered_cla(Some(cap), Acceptance::Accepted);

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
        let (cla, dispatched) = registered_cla(Some(cap), Acceptance::Accepted);

        let total_len = inner.len() as u64;
        let mut stream = inner;
        let result = cla
            .forward(None, &tunnel_addr(), &bundle_id, total_len, &mut stream)
            .await;

        assert!(matches!(result, Ok(ForwardOutcome::Sent)));
        assert_eq!(dispatched.load(Ordering::Relaxed), 1);
    }

    /// A BPA refusal of the outer bundle is this bundle's verdict, surfaced
    /// as an error the dispatcher parks per-bundle — never `NoNeighbour`.
    #[tokio::test]
    async fn bpa_refusal_is_not_no_neighbour() {
        let (cla, dispatched) = registered_cla(None, Acceptance::Refused);

        let (id, mut inner) = inner_bundle();
        let total_len = inner.len() as u64;
        let result = cla
            .forward(None, &tunnel_addr(), &id, total_len, &mut inner)
            .await;

        assert!(matches!(result, Err(ClaError::Internal(_))));
        assert_eq!(dispatched.load(Ordering::Relaxed), 1);
    }
}
