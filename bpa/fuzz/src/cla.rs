use super::*;

pub fn test_cla(data: hardy_bpa::Bytes) {
    // Full lifecycle
    get_runtime().block_on(async {
        // New BPA
        let bpa = hardy_bpa::bpa::Bpa::start(&hardy_bpa::config::Config {
            status_reports: true,
            node_ids: [hardy_bpv7::eid::Eid::Ipn {
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
            "ipn:*.*".parse().unwrap(),
            hardy_bpa::routes::Action::Via("ipn:0.2.0".parse().unwrap()),
            1,
        )
        .await;

        bpa.add_route(
            "fuzz".to_string(),
            "dtn://**/**".parse().unwrap(),
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
            let cla = std::sync::Arc::new(null_cla::NullCla::default());
            bpa.register_cla("fuzz".to_string(), None, cla.clone())
                .await
                .expect("Failed to register CLA");

            _ = cla.dispatch(data).await;

            cla.unregister().await;
        }

        bpa.shutdown().await;
    });
}

#[cfg(test)]
mod test {
    use std::io::Read;

    #[test]
    fn test() {
        if let Ok(mut file) =
            std::fs::File::open("./artifacts/cla/crash-4172e046d6370086ed8cd40e39103a772cc5b6be")
        {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                super::test_cla(buffer.into());
            }
        }
    }

    #[test]
    fn test_all() {
        match std::fs::read_dir("./corpus/cla") {
            Err(e) => {
                eprintln!(
                    "Failed to open dir: {e}, curr dir: {}",
                    std::env::current_dir().unwrap().to_string_lossy()
                );
            }
            Ok(dir) => {
                let mut count = 0u64;
                for entry in dir {
                    if let Ok(path) = entry {
                        let path = path.path();
                        if path.is_file() {
                            if let Ok(mut file) = std::fs::File::open(&path) {
                                let mut buffer = Vec::new();
                                if file.read_to_end(&mut buffer).is_ok() {
                                    super::test_cla(buffer.into());

                                    count = count.saturating_add(1);
                                    if count % 100 == 0 {
                                        tracing::info!("Processed {count} bundles");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
