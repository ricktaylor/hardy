mod cla;
mod eid;
mod service;

#[cfg(test)]
mod test;

use arbitrary::Arbitrary;
use std::sync::Arc;

#[derive(Arbitrary)]
enum Msg {
    Cla(cla::RandomBundle),
    ClaBytes(Vec<u8>),
    Service(service::Msg),
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
                None,
                Some(tracing::Level::INFO),
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

    // Metadata storage configuration
    cfg_if::cfg_if! {
        if #[cfg(feature = "sqlite-storage")] {
            let metadata_storage = Some(hardy_sqlite_storage::new(
                &hardy_sqlite_storage::Config {
                    db_dir: path.clone(),
                    db_name: "sqlite-storage.db".to_string(),
                },
                true
            ));
        } else {
            let metadata_storage = Some(hardy_bpa::storage::metadata_mem::new(
                &hardy_bpa::storage::metadata_mem::Config {
                    max_bundles: std::num::NonZero::new(1024).unwrap(),
                },
            ));
        }
    }

    // Bundle storage configuration
    cfg_if::cfg_if! {
        if #[cfg(feature = "localdisk-storage")] {
            let bundle_storage = Some(hardy_localdisk_storage::new(
                &hardy_localdisk_storage::Config {
                    store_dir: path.join("localdisk"),
                },
                true,
            ));
        } else {
            let bundle_storage = Some(hardy_bpa::storage::bundle_mem::new(
                &hardy_bpa::storage::bundle_mem::Config {
                    capacity: std::num::NonZero::new(524_288).unwrap(),
                    ..Default::default()
                }
            ));
        }
    }

    // New BPA
    let bpa = hardy_bpa::bpa::Bpa::start(
        &hardy_bpa::config::Config {
            status_reports: true,
            node_ids: [hardy_bpv7::eid::Eid::Ipn {
                allocator_id: 0,
                node_number: 1,
                service_number: 0,
            }]
            .as_slice()
            .try_into()
            .unwrap(),
            metadata_storage,
            bundle_storage,
            ..Default::default()
        },
        #[cfg(all(feature = "localdisk-storage", feature = "sqlite-storage"))]
        true,
        #[cfg(not(all(feature = "localdisk-storage", feature = "sqlite-storage")))]
        false,
    )
    .await
    .expect("Failed to start BPA");

    #[cfg(feature = "file-cla")]
    bpa.register_cla(
        "file-cla".to_string(),
        None,
        std::sync::Arc::new(hardy_file_cla::Cla::new(
            "file-cla".to_string(),
            hardy_file_cla::Config {
                outbox: None,
                peers: [("ipn:0.3.0".parse().unwrap(), path.join("inbox"))].into(),
            },
        )),
        None,
    )
    .await
    .expect("Failed to register CLA");

    bpa
}

impl Msg {
    fn send(self: Msg) {
        static PIPE: std::sync::OnceLock<flume::Sender<Msg>> = std::sync::OnceLock::new();
        PIPE.get_or_init(|| {
            let (tx, rx) = flume::bounded::<Msg>(0);

            get_runtime().spawn(async move {
                let bpa = new_bpa("fuzz").await;

                let cla = std::sync::Arc::new(cla::NullCla::default());
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

                let service = Arc::new(service::PipeService::default());
                bpa.register_service(None, service.clone())
                    .await
                    .expect("Failed to register service");

                // Now pull from the channel
                while let Ok(msg) = rx.recv_async().await {
                    match msg {
                        Msg::Cla(bundle) => {
                            _ = cla.dispatch(bundle.into_bundle()).await;
                        }
                        Msg::ClaBytes(bytes) => {
                            _ = cla.dispatch(bytes.into()).await;
                        }
                        Msg::Service(msg) => {
                            _ = service
                                .send(
                                    msg.destination.0,
                                    &msg.payload,
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
