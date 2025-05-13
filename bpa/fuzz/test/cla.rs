#![cfg(test)]

use crate::get_runtime;
use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use std::{io::Read, sync::Arc};

#[derive(Default)]
struct NullCla {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::cla::Sink>>,
}

impl NullCla {
    async fn dispatch(&self, data: &[u8]) -> hardy_bpa::cla::Result<()> {
        self.sink.get().unwrap().dispatch(data).await
    }

    async fn unregister(&self) {
        self.sink.get().unwrap().unregister().await
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for NullCla {
    async fn on_register(&self, _ident: String, sink: Box<dyn hardy_bpa::cla::Sink>) {
        if self.sink.set(sink).is_err() {
            panic!("Double connect()");
        }
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _destination: &bpv7::Eid,
        _data: &[u8],
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        todo!()
    }
}

fn test_cla(data: &[u8]) {
    // Full lifecycle
    get_runtime().block_on(async {
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
            "ipn:*.*.*|dtn://**/**".parse().unwrap(),
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
            bpa.register_cla("fuzz", cla.clone()).await;

            _ = cla.dispatch(data).await;

            cla.unregister().await;
        }

        bpa.shutdown().await;
    });
}

#[test]
fn test() {
    if let Ok(mut file) =
        std::fs::File::open("../artifacts/cla/slow-unit-0b540e80eea850ccb06685fbbae71dce6b87ff39")
    {
        let mut buffer = Vec::new();
        if file.read_to_end(&mut buffer).is_ok() {
            test_cla(&buffer);
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
                                test_cla(&buffer);
                            }
                        }
                    }
                }
            }
        }
    }
}
