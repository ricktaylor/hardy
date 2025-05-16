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
    async fn dispatch(&self, bundle: &[u8]) -> hardy_bpa::cla::Result<()> {
        self.sink.get().unwrap().dispatch(bundle).await
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for NullCla {
    async fn on_register(&self, sink: Box<dyn hardy_bpa::cla::Sink>) {
        if self.sink.set(sink).is_err() {
            panic!("Double connect()");
        }
    }

    async fn on_unregister(&self) {
        todo!()
    }

    async fn on_forward(
        &self,
        _destination: &bpv7::Eid,
        _bundle: &[u8],
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        todo!()
    }
}

fn start() -> Arc<NullCla> {
    let (tx, rx) = tokio::sync::oneshot::channel();

    get_runtime().spawn(async move {
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

        let cla = Arc::new(NullCla::default());
        bpa.register_cla("test".to_string(), cla.clone())
            .await
            .unwrap();

        tx.send(cla)
    });

    get_runtime().block_on(async move { rx.await.unwrap() })
}

#[test]
fn test() {
    let cla = start();

    get_runtime().block_on(async move {
        if let Ok(mut file) =
            std::fs::File::open("./artifacts/ingress/oom-e00b48801c97d3e554583d3c26fb742f9e6557ba")
        {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                _ = get_runtime()
                    .spawn(async move {
                        _ = cla.dispatch(&buffer).await;
                    })
                    .await;
            }
        }
    });
}

#[test]
fn test_all() {
    let cla = start();

    match std::fs::read_dir("./corpus/ingress") {
        Err(e) => {
            eprintln!(
                "Failed to open dir: {e}, curr dir: {}",
                std::env::current_dir().unwrap().to_string_lossy()
            );
        }
        Ok(dir) => {
            get_runtime().block_on(async {
                for entry in dir {
                    if let Ok(entry) = entry {
                        let path = entry.path();
                        if path.is_file() {
                            if let Ok(mut file) = std::fs::File::open(&path) {
                                let mut buffer = Vec::new();
                                if file.read_to_end(&mut buffer).is_ok() {
                                    let cla = cla.clone();
                                    _ = get_runtime()
                                        .spawn(async move {
                                            _ = cla.dispatch(&buffer).await;
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }
            });
        }
    }
}
