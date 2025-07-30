use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;

#[derive(Default)]
pub struct PipeService {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::service::Sink>>,
}

impl PipeService {
    pub async fn send(
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

    pub async fn unregister(&self) {
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
