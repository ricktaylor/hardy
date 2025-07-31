use super::*;
use arbitrary::Arbitrary;
use hardy_bpa::async_trait;
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
}

#[derive(Default)]
struct PipeService {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::service::Sink>>,
}

impl PipeService {
    async fn send(
        &self,
        destination: Eid,
        data: &[u8],
        lifetime: std::time::Duration,
        flags: Option<hardy_bpa::service::SendFlags>,
    ) -> hardy_bpa::service::Result<Box<str>> {
        self.sink
            .get()
            .unwrap()
            .send(destination, data, lifetime, flags)
            .await
    }

    async fn unregister(&self) {
        self.sink.get().unwrap().unregister().await
    }
}

#[async_trait]
impl hardy_bpa::service::Service for PipeService {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn hardy_bpa::service::Sink>) {
        if self.sink.set(sink).is_err() {
            panic!("Double connect()");
        }
    }

    async fn on_unregister(&self) {
        if self.sink.get().is_none() {
            panic!("Extra unregister!");
        }
    }

    async fn on_receive(&self, _bundle: hardy_bpa::service::Bundle) {
        // Do nothing
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &str,
        _kind: hardy_bpa::service::StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<hardy_bpv7::dtn_time::DtnTime>,
    ) {
        // Do nothing
    }
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
                let service = Arc::new(PipeService::default());

                bpa.register_service(None, service.clone())
                    .await
                    .expect("Failed to register service");

                // Now pull from the channel
                while let Some(msg) = rx.recv().await {
                    if let Ok(destination) = msg.destination.as_ref().parse() {
                        _ = service
                            .send(
                                destination,
                                "This is just fuzz data".as_bytes(),
                                msg.lifetime,
                                msg.flags.map(Into::into),
                            )
                            .await;
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

pub fn service_send(data: &[u8]) {
    if let Ok(msg) = Msg::arbitrary(&mut arbitrary::Unstructured::new(data)) {
        if msg.destination.as_ref().parse::<Eid>().is_ok() {
            send(msg);
        }
    }
}

// Use this to build a corpus of valid messages as a minimum
pub fn seed_msg(data: &[u8]) {
    if let Ok(msg) = Msg::arbitrary(&mut arbitrary::Unstructured::new(data)) {
        _ = msg.destination.as_ref().parse::<Eid>();
    }
}

#[cfg(test)]
mod test {
    use std::io::Read;

    #[test]
    fn test() {
        if let Ok(mut file) = std::fs::File::open(
            "./artifacts/service/crash-4172e046d6370086ed8cd40e39103a772cc5b6be",
        ) {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                super::service_send(&buffer);
            }
        }
    }

    #[test]
    fn test_all() {
        match std::fs::read_dir("./corpus/service") {
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
                                    super::service_send(&buffer);

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
