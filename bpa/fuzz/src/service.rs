use core::time::Duration;
use std::sync::Arc;

// This module's `SendOptions` is an `Arbitrary` mirror of the BPA type,
// so the BPA's own stays qualified.
use hardy_bpa::{
    Bytes, async_trait,
    service::{self, Service, Sink, StatusNotify},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use time::OffsetDateTime;

use super::*;

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

impl From<SendOptions> for service::SendOptions {
    fn from(val: SendOptions) -> Self {
        service::SendOptions {
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

pub struct PipeService {
    sink: Arc<dyn Sink>,
}

impl PipeService {
    pub fn factory() -> impl service::Factory<Component = Self> {
        service::factory_fn(|cx: service::Context| Ok(Arc::new(Self { sink: cx.sink })))
    }

    pub async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: Duration,
        options: Option<service::SendOptions>,
    ) -> service::Result<Id> {
        self.sink.send(destination, data, lifetime, options).await
    }
}

#[async_trait]
impl Service for PipeService {
    async fn deliver(
        &self,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        Ok(())
    }

    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}
