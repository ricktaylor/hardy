#![cfg(test)]

use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use std::{io::Read, sync::Arc};

use crate::get_runtime;

struct NullCla {
    sink: tokio::sync::Mutex<Option<Box<dyn hardy_bpa::cla::Sink>>>,
}

impl NullCla {
    fn new() -> Self {
        Self {
            sink: tokio::sync::Mutex::new(None),
        }
    }

    async fn dispatch(&self, data: &[u8]) {
        let mut guard = self.sink.lock().await;

        let sink = loop {
            if let Some(sink) = guard.as_ref() {
                break sink;
            }

            drop(guard);

            tokio::task::yield_now().await;

            guard = self.sink.lock().await;
        };

        _ = sink.dispatch(data).await;
    }

    async fn disconnect(&self) {
        let sink = self.sink.lock().await.take();
        if let Some(sink) = sink {
            _ = sink.disconnect().await;
        }
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for NullCla {
    async fn on_connect(&self, _ident: &str, sink: Box<dyn hardy_bpa::cla::Sink>) {
        self.sink.lock().await.replace(sink);
    }

    async fn on_disconnect(&self) {
        *self.sink.lock().await = None;
    }

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

        let cla = Arc::new(NullCla::new());
        bpa.register_cla("fuzz", cla.clone()).await;

        cla.dispatch(data).await;

        cla.disconnect().await;

        bpa.shutdown().await;
    });
}

#[test]
fn test() {
    test_cla(include_bytes!(
        "../artifacts/cla/slow-unit-0b540e80eea850ccb06685fbbae71dce6b87ff39"
    ));
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
