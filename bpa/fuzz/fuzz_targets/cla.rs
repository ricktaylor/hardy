#![no_main]

use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use libfuzzer_sys::fuzz_target;
use std::sync::Arc;

static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

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
        if let Some(sink) = self.sink.lock().await.take() {
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

fn setup() -> tokio::runtime::Runtime {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
        .with_target(true)
        .init();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fuzz_target!(|data: &[u8]| {
    // Full lifecycle
    RT.get_or_init(setup).block_on(async {
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
});

// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/cla/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/cla -o ./fuzz/coverage/cla/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/cla/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/cla -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/cla/lcov.info
