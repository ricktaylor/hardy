use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use std::sync::Arc;

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
        _node_ids: &[bpv7::Eid],
    ) -> hardy_bpa::cla::Result<()> {
        sink.add_peer(
            bpv7::Eid::Ipn {
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

        sink.remove_peer(&bpv7::Eid::Ipn {
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

pub fn test_cla(data: Vec<u8>) {
    // Full lifecycle
    super::get_runtime().block_on(async {
        // New BPA
        let bpa = hardy_bpa::bpa::Bpa::start(&hardy_bpa::config::Config {
            status_reports: true,
            node_ids: [bpv7::Eid::Ipn {
                allocator_id: 0,
                node_number: 1,
                service_number: 0,
            }]
            .as_slice()
            .try_into()
            .unwrap(),
            ..Default::default()
        })
        .await;

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
            hardy_bpa::routes::Action::Store(
                time::OffsetDateTime::parse(
                    "2035-01-02T11:12:13Z",
                    &time::format_description::well_known::Rfc3339,
                )
                .unwrap(),
            ),
            100,
        )
        .await;

        {
            let cla = Arc::new(NullCla::default());
            bpa.register_cla("fuzz".to_string(), None, cla.clone())
                .await
                .unwrap();

            _ = cla.dispatch(data.into()).await;

            cla.unregister().await;
        }

        bpa.shutdown().await;
    });
}

#[cfg(test)]
mod test {
    use std::io::Read;

    #[test]
    fn test() {
        if let Ok(mut file) =
            std::fs::File::open("./artifacts/cla/crash-19bbc54cc0df767008ca30335dbfdf7e040f7d4c")
        {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                super::test_cla(buffer);
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
                for entry in dir {
                    if let Ok(path) = entry {
                        let path = path.path();
                        if path.is_file() {
                            if let Ok(mut file) = std::fs::File::open(&path) {
                                let mut buffer = Vec::new();
                                if file.read_to_end(&mut buffer).is_ok() {
                                    super::test_cla(buffer);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
