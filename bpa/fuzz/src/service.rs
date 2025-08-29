use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;

#[derive(Arbitrary)]
pub struct SendFlags {
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
pub struct Msg {
    pub destination: eid::ArbitraryEid,
    pub lifetime: std::time::Duration,
    pub flags: Option<SendFlags>,
    pub payload: Vec<u8>,
}

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
