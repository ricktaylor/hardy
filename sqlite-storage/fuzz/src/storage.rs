use super::*;
use hardy_bpv7::eid::Eid;
use std::sync::Arc;
use tokio::sync::mpsc;

struct Msg {
    destination: Eid,
    data: hardy_bpa::Bytes,
    lifetime: std::time::Duration,
    flags: Option<hardy_bpa::service::SendFlags>,
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

            {
                let service = Arc::new(pipe_service::PipeService::default());

                bpa.register_service(None, service.clone())
                    .await
                    .expect("Failed to register service");

                // Now pull from the channel
                while let Some(msg) = rx.recv().await {
                    service
                        .send(msg.destination, &msg.data, msg.lifetime, msg.flags)
                        .await
                        .expect("Failed to send service message");
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

pub fn test_storage(data: hardy_bpa::Bytes) {
    // Full lifecycle
    send(Msg {
        destination: Eid::Ipn {
            allocator_id: 0,
            node_number: 99,
            service_number: 1,
        },
        data,
        lifetime: std::time::Duration::new(60, 0),
        flags: Some(hardy_bpa::service::SendFlags {
            do_not_fragment: true,
            ..Default::default()
        }),
    })
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
                super::test_storage(buffer.into());
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
                for entry in dir {
                    if let Ok(path) = entry {
                        let path = path.path();
                        if path.is_file() {
                            if let Ok(mut file) = std::fs::File::open(&path) {
                                let mut buffer = Vec::new();
                                if file.read_to_end(&mut buffer).is_ok() {
                                    super::test_storage(buffer.into());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
