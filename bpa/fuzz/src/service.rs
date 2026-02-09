use super::*;
use hardy_bpa::async_trait;
use hardy_bpv7::eid::Eid;

#[derive(Arbitrary)]
pub struct SendOptions {
    do_not_fragment: bool,
    request_ack: bool,
    report_status_time: bool,
    notify_reception: bool,
    notify_forwarding: bool,
    notify_delivery: bool,
    notify_deletion: bool,
}

impl From<SendOptions> for hardy_bpa::services::SendOptions {
    fn from(val: SendOptions) -> Self {
        hardy_bpa::services::SendOptions {
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
    pub flags: Option<SendOptions>,
    pub payload: Vec<u8>,
}

#[derive(Default)]
pub struct PipeService {
    sink: std::sync::OnceLock<Box<dyn hardy_bpa::services::ApplicationSink>>,
}

impl PipeService {
    pub async fn send(
        &self,
        destination: Eid,
        data: hardy_bpa::Bytes,
        lifetime: core::time::Duration,
        options: Option<hardy_bpa::services::SendOptions>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::bundle::Id> {
        self.sink
            .get()
            .unwrap()
            .send(destination, data, lifetime, options)
            .await
    }
}

#[async_trait]
impl hardy_bpa::services::Application for PipeService {
    async fn on_register(
        &self,
        _source: &Eid,
        sink: Box<dyn hardy_bpa::services::ApplicationSink>,
    ) {
        if self.sink.set(sink).is_err() {
            panic!("Double connect()");
        }
    }

    async fn on_unregister(&self) {
        if self.sink.get().is_none() {
            panic!("Extra unregister!");
        }
    }

    async fn on_receive(
        &self,
        _source: Eid,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _payload: hardy_bpa::Bytes,
    ) {
        // Do nothing
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &Eid,
        _kind: hardy_bpa::services::StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
        // Do nothing
    }
}
