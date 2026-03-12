mod cla;
mod eid;
mod service;

#[cfg(test)]
mod test;

use arbitrary::Arbitrary;
use hardy_bpa::bpa::BpaRegistration;
use std::sync::Arc;

#[derive(Arbitrary)]
enum Msg {
    Cla(cla::RandomBundle),
    ClaBytes(Vec<u8>),
    Service(service::Msg),
    // TODO: Implement Msg::TickTimer to advance mock clock and test expiry logic.
    // TODO: Implement Msg::UpdateRoute to test dynamic routing changes during processing.
}

fn get_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<(tokio::runtime::Runtime, hardy_otel::OtelGuard)> =
        std::sync::OnceLock::new();
    &RT.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let guard = rt.block_on(async {
            hardy_otel::init(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                tracing::Level::INFO,
            )
        });
        (rt, guard)
    })
    .0
}

#[allow(unused)]
async fn new_bpa(testname: &str) -> hardy_bpa::bpa::Bpa {
    let path =
        std::path::Path::new(&std::env::var("CARGO_TARGET_DIR").unwrap_or("fuzz".to_string()))
            .join("store")
            .join(testname);

    #[cfg(feature = "sqlite-storage")]
    let metadata_storage: Option<Arc<dyn hardy_bpa::storage::MetadataStorage>> = Some(
        hardy_sqlite_storage::new(
        &hardy_sqlite_storage::Config {
            db_dir: path.clone(),
            db_name: "sqlite-storage.db".to_string(),
        },
        true,
    ));
    #[cfg(not(feature = "sqlite-storage"))]
    let metadata_storage: Option<Arc<dyn hardy_bpa::storage::MetadataStorage>> = None;

    #[cfg(feature = "localdisk-storage")]
    let bundle_storage = Some(hardy_localdisk_storage::new(
        &hardy_localdisk_storage::Config {
            store_dir: path.join("localdisk"),
            fsync: false,
        },
        true,
    ));

    #[cfg(feature = "postgres-storage")]
    let metadata_storage: Option<Arc<dyn hardy_bpa::storage::MetadataStorage>> = Some(
        hardy_postgres_storage::new(
            &hardy_postgres_storage::Config {
                database_url: "postgres://hardy:hardy@192.168.2.3:5432/hardy".to_string(),
                ..Default::default()
            },
            true,
        )
        .await
        .expect("Failed to create postgres metadata storage"),
    );

    #[cfg(not(feature = "localdisk-storage"))]
    let bundle_storage = None;

    let bpa_config = hardy_bpa::config::Config {
        status_reports: true,
        node_ids: [hardy_bpv7::eid::NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice()
        .try_into()
        .unwrap(),
        ..Default::default()
    };

    let bpa = hardy_bpa::bpa::Bpa::new(bpa_config, metadata_storage, bundle_storage);

    bpa.start(
        #[cfg(all(feature = "localdisk-storage", feature = "sqlite-storage"))]
        true,
        #[cfg(not(all(feature = "localdisk-storage", feature = "sqlite-storage")))]
        false,
    );

    #[cfg(feature = "file-cla")]
    {
        let cla = std::sync::Arc::new(
            hardy_file_cla::Cla::new(&hardy_file_cla::Config {
                outbox: None,
                peers: [("ipn:0.3.0".parse().unwrap(), path.join("inbox"))].into(),
            })
            .expect("Failed to create file CLA"),
        );
        cla.register(&bpa, "file-cla".to_string())
            .await
            .expect("Failed to register CLA");
    }

    bpa
}

impl Msg {
    fn send(self) {
        static PIPE: std::sync::OnceLock<flume::Sender<Msg>> = std::sync::OnceLock::new();
        PIPE.get_or_init(|| {
            let (tx, rx) = flume::bounded::<Msg>(0);

            get_runtime().spawn(async move {
                let bpa = new_bpa("fuzz").await;

                let cla = std::sync::Arc::new(cla::NullCla::new());
                bpa.register_cla("fuzz".to_string(), None, cla.clone(), None)
                    .await
                    .expect("Failed to register CLA");

                // Load static routes
                bpa.add_route(
                    "fuzz".to_string(),
                    "ipn:*.*".parse().unwrap(),
                    hardy_bpa::routes::Action::Via("ipn:0.2.0".parse().unwrap()),
                    30,
                )
                .await;

                bpa.add_route(
                    "fuzz".to_string(),
                    "dtn://drop/**".parse().unwrap(),
                    hardy_bpa::routes::Action::Drop(Some(
                        hardy_bpv7::status_report::ReasonCode::NoKnownRouteToDestinationFromHere,
                    )),
                    50,
                )
                .await;

                bpa.add_route(
                    "fuzz".to_string(),
                    "dtn://drop2/**".parse().unwrap(),
                    hardy_bpa::routes::Action::Drop(None),
                    50,
                )
                .await;

                bpa.add_route(
                    "fuzz".to_string(),
                    "dtn://**/**".parse().unwrap(),
                    hardy_bpa::routes::Action::Reflect,
                    100,
                )
                .await;

                let service = Arc::new(service::PipeService::new());
                bpa.register_application(None, service.clone())
                    .await
                    .expect("Failed to register service");

                // Now pull from the channel
                while let Ok(msg) = rx.recv_async().await {
                    match msg {
                        Msg::Cla(bundle) => {
                            if let Ok(b) = bundle.into_bundle() {
                                _ = cla.dispatch(b).await;
                            }
                        }
                        Msg::ClaBytes(bytes) => {
                            _ = cla.dispatch(bytes.into()).await;
                        }
                        Msg::Service(msg) => {
                            _ = service
                                .send(
                                    msg.destination.0,
                                    msg.payload.into(),
                                    msg.lifetime,
                                    msg.flags.map(Into::into),
                                )
                                .await;
                        }
                    }
                }

                bpa.shutdown().await;
            });

            tx
        })
        .send(self)
        .expect("Send failed")
    }
}

pub fn send_random(data: &[u8]) -> bool {
    if let Ok(msg) = Msg::arbitrary(&mut arbitrary::Unstructured::new(data)) {
        msg.send();
        true
    } else {
        false
    }
}

pub fn send_bundle(data: &[u8]) {
    Msg::ClaBytes(data.to_vec()).send()
}
