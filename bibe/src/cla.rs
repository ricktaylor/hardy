use super::*;
use hardy_bpa::cla::{Cla, ClaAddress, ClaContext, ForwardBundleResult};

pub struct BibeCla {
    tunnel_source: Eid,
    ctx: Once<ClaContext>,
}

impl BibeCla {
    pub fn new(tunnel_source: Eid) -> Self {
        Self {
            tunnel_source,
            ctx: Once::new(),
        }
    }

    pub async fn add_tunnel(&self, tunnel_id: NodeId, decap_endpoint: Eid) -> Result<(), Error> {
        let cbor_bytes = hardy_cbor::encode::emit(&decap_endpoint).0;
        let cla_addr = ClaAddress::Private(cbor_bytes.into());

        self.ctx
            .get()
            .ok_or(Error::NotRegistered)?
            .add_peer(cla_addr, vec![tunnel_id]);

        Ok(())
    }

    pub(crate) fn dispatch(&self, bundle: Bytes) -> Result<(), Error> {
        self.ctx
            .get()
            .ok_or(Error::NotRegistered)?
            .dispatch(bundle, None, None);
        Ok(())
    }

    fn encapsulate(&self, inner: Bytes, outer_dest: Eid) -> Result<Bytes, Error> {
        let parsed = ParsedBundle::parse(&inner, bpsec::no_keys)?;
        let lifetime = parsed.bundle.lifetime;

        let payload = hardy_cbor::encode::emit_array(Some(4), |a| {
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(inner.as_ref());
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
    async fn on_register(&self, ctx: ClaContext, _node_ids: &[NodeId]) {
        self.ctx.call_once(|| ctx);
        debug!("BIBE CLA registered");
    }

    async fn on_unregister(&self) {
        debug!("BIBE CLA unregistered");
    }

    async fn forward(
        &self,
        _queue: Option<u32>,
        cla_addr: &ClaAddress,
        bundle: Bytes,
    ) -> hardy_bpa::cla::Result<ForwardBundleResult> {
        let ClaAddress::Private(addr_bytes) = cla_addr else {
            return Ok(ForwardBundleResult::NoNeighbour);
        };

        let outer_dest: Eid = hardy_cbor::decode::parse(addr_bytes)
            .map_err(|e: hardy_bpv7::eid::Error| hardy_bpa::cla::Error::Internal(e.into()))?;

        let encapsulated = self
            .encapsulate(bundle, outer_dest)
            .map_err(|e| hardy_bpa::cla::Error::Internal(e.into()))?;

        self.dispatch(encapsulated)
            .map_err(|e| hardy_bpa::cla::Error::Internal(e.into()))?;

        Ok(ForwardBundleResult::Sent)
    }
}
