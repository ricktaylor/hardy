use super::*;
use hardy_async::sync::spin::Once;
use hardy_bpa::async_trait;
use hardy_bpv7::eid::{IpnNodeId, NodeId};

#[derive(Arbitrary)]
pub struct RandomBundle {
    source: eid::ArbitraryEid,
    destination: eid::ArbitraryEid,
    report_to: Option<eid::ArbitraryEid>,
    flags: Option<u32>,
    crc_type: Option<u8>,
    lifetime: Option<core::time::Duration>,
    hop_limit: Option<(u64, u64)>,
    payload: Vec<u8>,
}

impl RandomBundle {
    pub fn into_bundle(self) -> Result<hardy_bpa::Bytes, hardy_bpv7::builder::Error> {
        let mut builder = hardy_bpv7::builder::Builder::new(self.source.0, self.destination.0);

        if let Some(report_to) = self.report_to {
            builder = builder.with_report_to(report_to.0);
        }

        if let Some(flags) = self.flags {
            builder = builder.with_flags((flags as u64).into());
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
    sink: Once<Box<dyn hardy_bpa::cla::Sink>>,
}

impl NullCla {
    pub fn new() -> Self {
        Self { sink: Once::new() }
    }
    pub async fn dispatch(&self, bundle: hardy_bpa::Bytes) -> hardy_bpa::cla::Result<()> {
        self.sink
            .get()
            .expect("dispatch called before registration")
            .dispatch(bundle, None, None)
            .await
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for NullCla {
    async fn on_register(&self, sink: Box<dyn hardy_bpa::cla::Sink>, _node_ids: &[NodeId]) {
        sink.add_peer(
            NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            }),
            hardy_bpa::cla::ClaAddress::Private("fuzz".as_bytes().into()),
        )
        .await
        .expect("add_peer failed");

        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        let Some(sink) = self.sink.get() else {
            panic!("Extra unregister!");
        };

        sink.remove_peer(
            NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            }),
            &hardy_bpa::cla::ClaAddress::Private("fuzz".as_bytes().into()),
        )
        .await
        .expect("remove_peer failed");
    }

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &hardy_bpa::cla::ClaAddress,
        _bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        Ok(hardy_bpa::cla::ForwardBundleResult::Sent)
    }
}
