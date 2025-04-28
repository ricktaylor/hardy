#![cfg(test)]

/*
use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use std::sync::Arc;

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
    async fn on_connect(&self, sink: Box<dyn hardy_bpa::cla::Sink>) -> hardy_bpa::cla::Result<()> {
        self.sink.lock().await.replace(sink);
        Ok(())
    }

    async fn on_disconnect(&self) {
        *self.sink.lock().await = None;
    }

    async fn forward(
        &self,
        _destination: &bpv7::Eid,
        _addr: Option<&[u8]>,
        _data: &[u8],
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        todo!()
    }
}

#[test]
fn test() {
    let data = include_bytes!("artifacts/cla/crash-da39a3ee5e6b4b0d3255bfef95601890afd80709");

    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
        .with_target(true)
        .init();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // New BPA
            let bpa = hardy_bpa::bpa::Bpa::start(hardy_bpa::bpa::Config {
                status_reports: true,
                admin_endpoints: vec![bpv7::Eid::Ipn {
                    allocator_id: 0,
                    node_number: 1,
                    service_number: 0,
                }],
                ..Default::default()
            })
            .await;

            // Load static routes
            bpa.add_forwarding_action(
                "fuzz",
                &"ipn:*.*.*|dtn://**/
**".parse().unwrap(),
                &hardy_bpa::fib::Action::Store(
                    time::OffsetDateTime::parse(
                        "2035-01-02T11:12:13Z",
                        &time::format_description::well_known::Rfc3339,
                    )
                    .unwrap(),
                ),
                100,
            )
            .await
            .unwrap();

            let cla = Arc::new(NullCla::new());
            bpa.register_cla("fuzz", "test", cla.clone()).await.unwrap();

            cla.dispatch(data).await;

            cla.disconnect().await;

            bpa.shutdown().await;
        });
}
*/
