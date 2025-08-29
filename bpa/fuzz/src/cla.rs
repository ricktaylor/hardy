use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;

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
    pub fn into_bundle(self) -> hardy_bpa::Bytes {
        let mut builder = hardy_bpv7::builder::Builder::new(self.source.0, self.destination.0);

        if let Some(report_to) = self.report_to {
            builder.with_report_to(report_to.0);
        }

        if let Some(flags) = self.flags {
            builder.with_flags((flags as u64).into());
        }

        if let Some(crc_type) = self.crc_type {
            builder.with_crc_type(match (crc_type as u64).into() {
                hardy_bpv7::crc::CrcType::Unrecognised(_) => hardy_bpv7::crc::CrcType::None,
                crc_type => crc_type,
            });
        }

        if let Some(lifetime) = self.lifetime {
            builder.with_lifetime(lifetime);
        }

        if let Some((limit, count)) = self.hop_limit {
            let mut builder = builder.add_extension_block(hardy_bpv7::block::Type::HopCount);
            builder.with_flags(hardy_bpv7::block::Flags {
                must_replicate: true,
                delete_bundle_on_failure: true,
                ..Default::default()
            });
            builder.build(hardy_cbor::encode::emit(&hardy_bpv7::hop_info::HopInfo {
                limit,
                count,
            }));
        }

        builder.build(self.payload).1.into()
    }
}

#[derive(Default)]
pub struct NullCla {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::cla::Sink>>,
}

impl NullCla {
    pub async fn dispatch(&self, bundle: hardy_bpa::Bytes) -> hardy_bpa::cla::Result<()> {
        self.sink.get().unwrap().dispatch(bundle).await
    }

    pub async fn unregister(&self) {
        self.sink.get().unwrap().unregister().await
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for NullCla {
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::cla::Sink>,
        _node_ids: &[Eid],
    ) -> hardy_bpa::cla::Result<()> {
        sink.add_peer(
            Eid::Ipn {
                allocator_id: 0,
                node_number: 2,
                service_number: 0,
            },
            hardy_bpa::cla::ClaAddress::Unknown(1, "fuzz".as_bytes().into()),
        )
        .await
        .expect("add_peer failed");

        if self.sink.set(sink).is_err() {
            panic!("Double connect()");
        }

        Ok(())
    }

    async fn on_unregister(&self) {
        let Some(sink) = self.sink.get() else {
            panic!("Extra unregister!");
        };

        sink.remove_peer(&Eid::Ipn {
            allocator_id: 0,
            node_number: 2,
            service_number: 0,
        })
        .await
        .expect("remove_peer failed");
    }

    async fn on_forward(
        &self,
        _cla_addr: hardy_bpa::cla::ClaAddress,
        _bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        // if bundle.len() > 1024 {
        //     return Ok(hardy_bpa::cla::ForwardBundleResult::TooBig(1024));
        // }

        Ok(hardy_bpa::cla::ForwardBundleResult::Sent)
    }
}
