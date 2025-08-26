use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;

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

pub fn cla_send(data: hardy_bpa::Bytes) {
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

#[cfg(test)]
mod test {
    use std::io::Read;

    #[test]
    fn test() {
        if let Ok(mut file) =
            std::fs::File::open("./artifacts/cla/crash-4172e046d6370086ed8cd40e39103a772cc5b6be")
        {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                super::cla_send(buffer.into());
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
                                    super::cla_send(buffer.into());

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
