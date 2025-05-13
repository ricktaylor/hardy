#![cfg(test)]

use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use std::{io::Read, sync::Arc};

use crate::get_runtime;

static SINK: std::sync::OnceLock<Box<dyn hardy_bpa::cla::Sink>> = std::sync::OnceLock::new();

struct NullCla {}

#[async_trait]
impl hardy_bpa::cla::Cla for NullCla {
    async fn on_register(&self, _ident: String, sink: Box<dyn hardy_bpa::cla::Sink>) {
        if SINK.set(sink).is_err() {
            panic!("Double connect()");
        }
    }

    async fn on_unregister(&self) {
        todo!()
    }

    async fn forward(
        &self,
        _destination: &bpv7::Eid,
        _data: &[u8],
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        todo!()
    }
}

fn start() {
    get_runtime().spawn(async {
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

        let cla = Arc::new(NullCla {});
        bpa.register_cla("test", cla).await;
    });
}

#[test]
fn test() {
    start();

    get_runtime().block_on(async {
        let sink = loop {
            if let Some(sink) = SINK.get() {
                break sink;
            }
            tokio::task::yield_now().await;
        };

        if let Ok(mut file) =
            std::fs::File::open("./artifacts/ingress/oom-e00b48801c97d3e554583d3c26fb742f9e6557ba")
        {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                _ = get_runtime()
                    .spawn(async move {
                        _ = sink.dispatch(&buffer).await;
                    })
                    .await;
            }
        }
    });
}

#[test]
fn test_all() {
    start();

    let sink = get_runtime().block_on(async {
        loop {
            if let Some(sink) = SINK.get() {
                break sink;
            }
            tokio::task::yield_now().await;
        }
    });

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
                                    _ = get_runtime()
                                        .spawn(async move {
                                            _ = sink.dispatch(&buffer).await;
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
