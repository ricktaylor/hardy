use super::*;
use arbitrary::Arbitrary;
use hardy_bpv7::eid::Eid;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Arbitrary)]
struct SendFlags {
    do_not_fragment: bool,
    request_ack: bool,
    report_status_time: bool,
    notify_reception: bool,
    notify_forwarding: bool,
    notify_delivery: bool,
    notify_deletion: bool,
}

impl Into<hardy_bpa::service::SendFlags> for SendFlags {
    fn into(self) -> hardy_bpa::service::SendFlags {
        hardy_bpa::service::SendFlags {
            do_not_fragment: self.do_not_fragment,
            request_ack: self.request_ack,
            report_status_time: self.report_status_time,
            notify_reception: self.notify_reception,
            notify_forwarding: self.notify_forwarding,
            notify_delivery: self.notify_delivery,
            notify_deletion: self.notify_deletion,
        }
    }
}

#[derive(Arbitrary)]
struct Msg {
    destination: Box<str>,
    lifetime: std::time::Duration,
    flags: Option<SendFlags>,
    data: Box<[u8]>,
}

fn send(msg: Msg) {
    static PIPE: std::sync::OnceLock<mpsc::Sender<Msg>> = std::sync::OnceLock::new();
    PIPE.get_or_init(|| {
        let (tx, mut rx) = mpsc::channel::<Msg>(16);

        get_runtime().spawn(async move {
            // New BPA
            let bpa = hardy_bpa::bpa::Bpa::start(&hardy_bpa::config::Config {
                status_reports: true,
                node_ids: [Eid::Ipn {
                    allocator_id: 0,
                    node_number: 1,
                    service_number: 0,
                }]
                .as_slice()
                .try_into()
                .unwrap(),
                metadata_storage: Some(hardy_sqlite_storage::new(
                    &hardy_sqlite_storage::Config {
                        db_dir: "fuzz".into(),
                        db_name: "storage.db".into(),
                        ..Default::default()
                    },
                    true,
                )),
                ..Default::default()
            })
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
                let service = Arc::new(pipe_service::PipeService::default());

                bpa.register_service(None, service.clone())
                    .await
                    .expect("Failed to register service");

                // Now pull from the channel
                while let Some(msg) = rx.recv().await {
                    if let Ok(destination) = msg.destination.as_ref().parse() {
                        service
                            .send(
                                destination,
                                &msg.data,
                                msg.lifetime,
                                msg.flags.map(Into::into),
                            )
                            .await
                            .expect("Failed to send service message");
                    }
                }

                service.unregister().await;
            }

            bpa.shutdown().await;
        });

        tx
    })
    .blocking_send(msg)
    .expect("Send failed")
}

pub fn test_storage(data: &[u8]) {
    if let Ok(msg) = Msg::arbitrary(&mut arbitrary::Unstructured::new(data)) {
        send(msg);
    }
}

#[cfg(test)]
mod test {
    use std::io::Read;

    #[test]
    fn test() {
        if let Ok(mut file) = std::fs::File::open(
            "./artifacts/storage/crash-4172e046d6370086ed8cd40e39103a772cc5b6be",
        ) {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                super::test_storage(&buffer);
            }
        }
    }

    #[test]
    fn test_all() {
        match std::fs::read_dir("./corpus/storage") {
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
                                    super::test_storage(&buffer);

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
