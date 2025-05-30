#![no_main]

use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use libfuzzer_sys::fuzz_target;
use std::sync::Arc;

static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

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
    async fn on_register(&self, sink: Box<dyn hardy_bpa::cla::Sink>, _node_ids: &[bpv7::Eid]) {
        if self.sink.set(sink).is_err() {
            panic!("Double connect()");
        }
    }

    async fn on_unregister(&self) {}

    async fn on_forward(
        &self,
        _cla_addr: hardy_bpa::cla::ClaAddress,
        _bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        unimplemented!()
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

        {
            let cla = Arc::new(NullCla::default());
            bpa.register_cla("fuzz".to_string(), None, cla.clone())
                .await
                .unwrap();

            _ = cla.dispatch(data.to_vec().into()).await;

            cla.unregister().await;
        }

        bpa.shutdown().await;
    });
});

// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/cla/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/cla -o ./fuzz/coverage/cla/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/cla/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/cla -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/cla/lcov.info
