use core::num::NonZeroU8;

use super::*;
use hardy_bpa::{
    async_trait,
    cla::{
        self, Acceptance, Address, Context, Factory, ForwardOutcome, Request, Segment, Sink,
        factory_fn,
    },
};
use hardy_bpv7::eid::{IpnNodeId, NodeId};

#[derive(Arbitrary)]
pub struct RandomBundle {
    source: eid::ArbitraryEid,
    destination: eid::ArbitraryEid,
    report_to: Option<eid::ArbitraryEid>,
    flags: Option<u32>,
    crc_type: Option<u8>,
    lifetime: Option<core::time::Duration>,
    hop_limit: Option<(NonZeroU8, u64)>,
    payload: Vec<u8>,
}

impl RandomBundle {
    pub fn into_bundle(self) -> Result<hardy_bpa::Bytes, hardy_bpv7::builder::Error> {
        let mut builder = hardy_bpv7::builder::Builder::new(self.source.0, self.destination.0);

        if let Some(report_to) = self.report_to {
            builder = builder.with_report_to(report_to.0);
        }

        if let Some(flags) = self.flags {
            let mut flags = hardy_bpv7::bundle::Flags::from(flags as u64);
            flags.is_fragment = false;
            builder = builder.with_flags(flags);
        }

        if let Some(crc_type) = self.crc_type {
            builder = builder.with_crc_type(match (crc_type as u64).into() {
                hardy_bpv7::crc::CrcType::Unrecognised(_) => hardy_bpv7::crc::CrcType::None,
                crc_type => crc_type,
            });
        }

        if let Some(lifetime) = self.lifetime {
            builder = builder.with_lifetime(lifetime);
        }

        if let Some((limit, count)) = self.hop_limit {
            builder = builder.with_hop_count(&hardy_bpv7::hop_info::HopInfo { limit, count });
        }

        builder
            .with_payload(self.payload.into())
            .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
            .map(|b| b.1.into())
    }
}

pub struct NullCla {
    sink: Arc<dyn Sink>,
}

impl NullCla {
    pub fn factory() -> impl Factory<Component = Self> {
        factory_fn(Request::default(), |cx: Context| {
            Ok(Arc::new(Self { sink: cx.sink }))
        })
    }

    pub async fn dispatch(&self, mut bundle: hardy_bpa::Bytes) -> cla::Result<Acceptance> {
        self.sink.dispatch(None, None, &mut bundle).await
    }

    fn peer_addr() -> Address {
        Address::Private("fuzz".as_bytes().into())
    }
}

#[async_trait]
impl cla::Cla for NullCla {
    async fn start(&self) -> cla::Result<()> {
        self.sink
            .add_peer(
                Self::peer_addr(),
                &[NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 2,
                })],
            )
            .await
            .map(|_| ())
    }

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &Address,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _total_len: u64,
        _stream: &mut dyn hardy_bpa::stream::Receiver<Segment>,
    ) -> cla::Result<ForwardOutcome> {
        Ok(ForwardOutcome::Sent)
    }
}
