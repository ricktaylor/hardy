use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;

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
        bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        if bundle.len() > 1024 {
            return Ok(hardy_bpa::cla::ForwardBundleResult::TooBig(1024));
        }

        Ok(hardy_bpa::cla::ForwardBundleResult::Sent)
    }
}
