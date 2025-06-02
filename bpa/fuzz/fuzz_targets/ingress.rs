#![no_main]

use hardy_bpa::async_trait;
use hardy_bpv7::prelude as bpv7;
use libfuzzer_sys::fuzz_target;
use std::sync::Arc;

#[derive(Default)]
struct NullCla {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::cla::Sink>>,
}

impl NullCla {
    async fn dispatch(&self, bundle: hardy_bpa::Bytes) -> hardy_bpa::cla::Result<()> {
        self.sink.get().unwrap().dispatch(bundle).await
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for NullCla {
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::cla::Sink>,
        _node_ids: &[bpv7::Eid],
    ) -> hardy_bpa::cla::Result<()> {
        if self.sink.set(sink).is_err() {
            panic!("Double connect()");
        }
        Ok(())
    }

    async fn on_unregister(&self) {
        unimplemented!()
    }

    async fn on_forward(
        &self,
        _cla_addr: hardy_bpa::cla::ClaAddress,
        _bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        unimplemented!()
    }
}

fn get_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with_target(true)
            .init();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    })
}

fn setup_cla() -> Arc<NullCla> {
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
        bpa.register_cla("test".to_string(), None, cla.clone())
            .await
            .unwrap();

        tx.send(cla)
    });

    get_runtime().block_on(async move { rx.await.unwrap() })
}

fuzz_target!(|data: &[u8]| {
    static CLA: std::sync::OnceLock<Arc<NullCla>> = std::sync::OnceLock::new();
    let cla = CLA.get_or_init(setup_cla);

    get_runtime().block_on(async {
        _ = cla.dispatch(data.to_vec().into()).await;
    })
});

// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/ingress/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/ingress -o ./fuzz/coverage/ingress/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/ingress/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/ingress -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/ingress/lcov.info
