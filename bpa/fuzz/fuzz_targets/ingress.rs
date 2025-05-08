#![no_main]

use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use libfuzzer_sys::fuzz_target;
use std::sync::Arc;

static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
static SINK: std::sync::OnceLock<Box<dyn hardy_bpa::cla::Sink>> = std::sync::OnceLock::new();

struct NullCla {}

#[async_trait]
impl hardy_bpa::cla::Cla for NullCla {
    async fn on_connect(&self, _ident: &str, sink: Box<dyn hardy_bpa::cla::Sink>) {
        if SINK.set(sink).is_err() {
            panic!("Double connect()");
        }
    }

    async fn on_disconnect(&self) {
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

fn setup() -> tokio::runtime::Runtime {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with_target(true)
        .init();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.spawn(async {
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

    rt
}

fn test_ingress(data: &[u8]) {
    RT.get_or_init(setup).block_on(async {
        let sink = loop {
            if let Some(sink) = SINK.get() {
                break sink;
            }
            tokio::task::yield_now().await;
        };

        let metrics = RT.get().unwrap().metrics();
        let cur_tasks = metrics.num_alive_tasks();

        let _ = sink.dispatch(data).await;

        // This is horrible, but ensures we actually reach the async parts...
        while metrics.num_alive_tasks() > cur_tasks {
            tokio::task::yield_now().await;
        }
    })
}

fuzz_target!(|data: &[u8]| {
    test_ingress(data);
});

/*
#[test]
fn test() {
    test_ingress(include_bytes!(
        "../artifacts/ingress/crash-da39a3ee5e6b4b0d3255bfef95601890afd80709"
    ));
}
*/

// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/ingress/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/ingress -o ./fuzz/coverage/ingress/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/ingress/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/ingress -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/ingress/lcov.info
