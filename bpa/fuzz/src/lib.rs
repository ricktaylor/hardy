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
    let path = std::env::temp_dir().join("hardy-fuzz").join(testname);

    let mut builder = hardy_bpa::bpa::Bpa::builder()
        .status_reports(true)
        .lru_capacity(core::num::NonZeroUsize::new(16).unwrap())
        .node_ids(
            [hardy_bpv7::eid::NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            })]
            .as_slice()
            .try_into()
            .unwrap(),
        );

    // Bundle storage
    cfg_if::cfg_if! {
        if #[cfg(feature = "localdisk-storage")] {
            builder = builder.bundle_storage(hardy_localdisk_storage::new(
                &hardy_localdisk_storage::Config {
                    store_dir: path.join("localdisk"),
                    fsync: false,
                },
                true,
            ));
        } else {
            builder = builder.bundle_storage(std::sync::Arc::new(
                hardy_bpa::storage::BundleMemStorage::new(
                    &hardy_bpa::storage::BundleMemStorageConfig {
                        capacity: core::num::NonZero::new(1_048_576).unwrap(), // 1 MB
                        min_bundles: 4,
                    },
                ),
            ));
        }
    }

    // Metadata storage
    cfg_if::cfg_if! {
        if #[cfg(feature = "sqlite-storage")] {
            builder = builder.metadata_storage(hardy_sqlite_storage::new(
                &hardy_sqlite_storage::Config {
                    db_dir: path.clone(),
                    db_name: "sqlite-storage.db".to_string(),
                },
                true,
            ));
        } else if #[cfg(feature = "postgres-storage")] {
            builder = builder.metadata_storage(
                hardy_postgres_storage::new(
                    &hardy_postgres_storage::Config {
                        database_url: "postgres://hardy:hardy@localhost:5432/hardy".to_string(),
                        ..Default::default()
                    },
                    true,
                )
                .await
                .expect("Failed to create postgres metadata storage"),
            );
        } else {
            builder = builder.metadata_storage(std::sync::Arc::new(
                hardy_bpa::storage::MetadataMemStorage::new(
                    &hardy_bpa::storage::MetadataMemStorageConfig {
                        max_bundles: core::num::NonZero::new(256).unwrap(),

                    },
                ),
            ));
        }
    }

    let bpa = builder.build().await.expect("Failed to build BPA");

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
        bpa.register_cla("file-cla".to_string(), cla)
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
                bpa.register_cla("fuzz".to_string(), cla.clone())
                    .await
                    .expect("Failed to register CLA");

                // Load static routes
                bpa.register_routing_agent(
                    "fuzz".to_string(),
                    Arc::new(hardy_bpa::routes::StaticRoutingAgent::new(&[
                        (
                            "ipn:*.*".parse().unwrap(),
                            hardy_bpa::routes::Action::Via("ipn:0.2.0".parse().unwrap()),
                            30,
                        ),
                        (
                            "dtn://drop/**".parse().unwrap(),
                            hardy_bpa::routes::Action::Drop(Some(
                                hardy_bpv7::status_report::ReasonCode::NoKnownRouteToDestinationFromHere,
                            )),
                            50,
                        ),
                        (
                            "dtn://drop2/**".parse().unwrap(),
                            hardy_bpa::routes::Action::Drop(None),
                            50,
                        ),
                        (
                            "dtn://**/**".parse().unwrap(),
                            hardy_bpa::routes::Action::Reflect,
                            100,
                        ),
                    ])),
                )
                .await
                .expect("Failed to register routing agent");

                let service = Arc::new(service::PipeService::new());
                bpa.register_application(hardy_bpv7::eid::Service::Ipn(1), service.clone())
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
