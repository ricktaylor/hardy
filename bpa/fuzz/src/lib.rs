pub mod cla;
pub mod service;

use arbitrary::Arbitrary;

struct ArbitraryEid(hardy_bpv7::eid::Eid);

impl<'a> Arbitrary<'a> for ArbitraryEid {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        if u.arbitrary::<bool>()? {
            let allocator_id = u.arbitrary()?;
            let node_number = u.arbitrary()?;
            let service_number = u.arbitrary()?;

            if allocator_id == 0 && node_number == 0 && service_number == 0 {
                Ok(ArbitraryEid(hardy_bpv7::eid::Eid::Null))
            } else if allocator_id == 0 && node_number == u32::MAX {
                Ok(ArbitraryEid(hardy_bpv7::eid::Eid::LocalNode {
                    service_number,
                }))
            } else {
                Ok(ArbitraryEid(hardy_bpv7::eid::Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number,
                }))
            }
        } else {
            let node_name: Box<str> = urlencoding::decode(u.arbitrary()?)
                .map_err(|_| arbitrary::Error::IncorrectFormat)?
                .into();
            if node_name.as_ref() == "none" {
                Ok(ArbitraryEid(hardy_bpv7::eid::Eid::Null))
            } else {
                let demux: String = u.arbitrary()?;
                if demux.contains(|c| c >= '\u{21}' && c <= '\u{7e}') {
                    Err(arbitrary::Error::IncorrectFormat)
                } else {
                    Ok(ArbitraryEid(hardy_bpv7::eid::Eid::Dtn {
                        node_name,
                        demux: demux.into(),
                    }))
                }
            }
        }
    }
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
                    store_dir: path.join("localdisk"),
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
