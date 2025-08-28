use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;
use std::sync::Arc;

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

impl From<SendFlags> for hardy_bpa::service::SendFlags {
    fn from(val: SendFlags) -> Self {
        hardy_bpa::service::SendFlags {
            do_not_fragment: val.do_not_fragment,
            request_ack: val.request_ack,
            report_status_time: val.report_status_time,
            notify_reception: val.notify_reception,
            notify_forwarding: val.notify_forwarding,
            notify_delivery: val.notify_delivery,
            notify_deletion: val.notify_deletion,
        }
    }
}

#[derive(Arbitrary)]
struct Msg {
    destination: ArbitraryEid,
    lifetime: std::time::Duration,
    flags: Option<SendFlags>,
    payload: Vec<u8>,
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
    static PIPE: std::sync::OnceLock<flume::Sender<Msg>> = std::sync::OnceLock::new();
    PIPE.get_or_init(|| {
        let (tx, rx) = flume::bounded::<Msg>(0);

        get_runtime().spawn(async move {
            // New BPA
            let bpa = new_bpa("service").await;

            {
                let service = Arc::new(PipeService::default());

                bpa.register_service(None, service.clone())
                    .await
                    .expect("Failed to register service");

                let mut count = 0u64;

                // Now pull from the channel
                while let Ok(msg) = rx.recv_async().await {
                    _ = service
                        .send(
                            msg.destination.0,
                            &msg.payload,
                            msg.lifetime,
                            msg.flags.map(Into::into),
                        )
                        .await;

                    count += 1;
                    tracing::event!(
                        target: "metrics",
                        tracing::Level::TRACE,
                        monotonic_counter.fuzz_service.dispatched_bundles = count
                    );
                }

                service.unregister().await;
            }

            bpa.shutdown().await;
        });

        tx
    })
    .send(msg)
    .expect("Send failed")
}

pub fn service_send(data: &[u8]) -> bool {
    if let Ok(msg) = Msg::arbitrary(&mut arbitrary::Unstructured::new(data)) {
        send(msg);
        true
    } else {
        false
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
                                }
                            }
                        }
                    }
                }
                tracing::info!("Processed {count} bundles");
            }
        }
    }
}
