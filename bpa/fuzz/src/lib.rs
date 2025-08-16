pub mod cla;
pub mod service;

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
    // Metadata storage configuration
    cfg_if::cfg_if! {
        if #[cfg(feature = "sqlite-storage")] {
            let metadata_storage = Some(hardy_sqlite_storage::new(
                &hardy_sqlite_storage::Config {
                    db_dir: std::path::Path::new("fuzz").join(testname),
                    db_name: "sqlite-storage.db".to_string(),
                },
                true
            ));
        } else {
            let metadata_storage = Some(hardy_bpa::metadata_mem::new(
                &hardy_bpa::metadata_mem::Config {
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
                    store_dir: std::path::Path::new("fuzz").join(testname).join("localdisk"),
                },
                true,
            ));
        } else {
            let bundle_storage = Some(hardy_bpa::bundle_mem::new(
                &hardy_bpa::bundle_mem::Config {
                    capacity: std::num::NonZero::new(524_288).unwrap(),
                    ..Default::default()
                }
            ));
        }
    }

    // New BPA
    hardy_bpa::bpa::Bpa::start(&hardy_bpa::config::Config {
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
    })
    .await
    .expect("Failed to start BPA")
}
