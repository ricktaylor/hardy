use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;

#[derive(Arbitrary)]
struct RandomBundle {
    source: ArbitraryEid,
    destination: ArbitraryEid,
    report_to: Option<ArbitraryEid>,
    flags: Option<u32>,
    crc_type: Option<u8>,
    lifetime: Option<core::time::Duration>,
    hop_limit: Option<(u64, u64)>,
    payload: Vec<u8>,
}

impl RandomBundle {
    fn into_bundle(self) -> hardy_bpa::Bytes {
        let mut builder = hardy_bpv7::builder::Builder::new(self.source.0, self.destination.0);

        if let Some(report_to) = self.report_to {
            builder.with_report_to(report_to.0);
        }

        if let Some(flags) = self.flags {
            builder.with_flags((flags as u64).into());
        }

        if let Some(crc_type) = self.crc_type {
            builder.with_crc_type((crc_type as u64).into());
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
struct NullCla {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::cla::Sink>>,
}

impl NullCla {
    async fn dispatch(&self, bundle: hardy_bpa::Bytes) -> hardy_bpa::cla::Result<()> {
        self.sink.get().unwrap().dispatch(bundle).await
    }

    async fn unregister(&self) {
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

fn send(data: hardy_bpa::Bytes) {
    static PIPE: std::sync::OnceLock<flume::Sender<hardy_bpa::Bytes>> = std::sync::OnceLock::new();
    PIPE.get_or_init(|| {
        let (tx, rx) = flume::bounded::<hardy_bpa::Bytes>(0);

        get_runtime().spawn(async move {
            let bpa = new_bpa("cla").await;

            // Load static routes
            bpa.add_route(
                "fuzz".to_string(),
                "ipn:*.*".parse().unwrap(),
                hardy_bpa::routes::Action::Via("ipn:0.2.0".parse().unwrap()),
                1,
            )
            .await;

            bpa.add_route(
                "fuzz".to_string(),
                "dtn://**/**".parse().unwrap(),
                hardy_bpa::routes::Action::Reflect,
                100,
            )
            .await;

            {
                let cla = std::sync::Arc::new(NullCla::default());
                bpa.register_cla("fuzz".to_string(), None, cla.clone())
                    .await
                    .expect("Failed to register CLA");

                let mut count = 0u64;

                // Now pull from the channel
                while let Ok(data) = rx.recv_async().await {
                    _ = cla.dispatch(data).await;

                    count += 1;
                    tracing::event!(
                        target: "metrics",
                        tracing::Level::TRACE,
                        monotonic_counter.fuzz_cla.dispatched_bundles = count
                    );
                }

                cla.unregister().await;
            }

            bpa.shutdown().await;
        });

        tx
    })
    .send(data)
    .expect("Send failed")
}

pub fn cla_send(data: &[u8]) -> bool {
    if let Ok(bundle) = RandomBundle::arbitrary(&mut arbitrary::Unstructured::new(data)) {
        send(bundle.into_bundle());
        true
    } else {
        false
    }
}

#[cfg(test)]
mod test {
    use std::io::Read;

    #[test]
    fn test() {
        if let Ok(mut file) =
            std::fs::File::open("./artifacts/cla/crash-5943b7c21c186171effb01f9514dbe9302f2a606")
        {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                super::cla_send(&buffer);
            }
        }
    }

    #[test]
    fn test_all() {
        match std::fs::read_dir("./corpus/cla") {
            Err(e) => {
                eprintln!(
                    "Failed to open dir: {e}, curr dir: {}",
                    std::env::current_dir().unwrap().to_string_lossy()
                );
            }
            Ok(dir) => {
                let mut count = 0u64;
                for entry in dir {
                    if let Ok(path) = entry {
                        let path = path.path();
                        if path.is_file() {
                            if let Ok(mut file) = std::fs::File::open(&path) {
                                let mut buffer = Vec::new();
                                if file.read_to_end(&mut buffer).is_ok() {
                                    super::cla_send(&buffer);

                                    count = count.saturating_add(1);
                                }
                            }
                        }
                    }
                }
                tracing::info!("Processed {count} bundles");
            }
        }
    }
}
