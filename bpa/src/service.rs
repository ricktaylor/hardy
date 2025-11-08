use super::*;
use hardy_bpv7::eid::Eid;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("There is already a service using ipn service number {0}")]
    IpnServiceInUse(u32),

    #[error("There is already a service using dtn service demux {0}")]
    DtnServiceInUse(String),

    #[error("There is no ipn node id configured")]
    NoIpnNodeId,

    #[error("There is no dtn node id configured")]
    NoDtnNodeId,

    #[error("The sink is disconnected")]
    Disconnected,

    #[error("Invalid bundle destination {0}")]
    InvalidDestination(Eid),

    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServiceId<'a> {
    DtnService(&'a str),
    IpnService(u32),
}

#[derive(Debug)]
pub enum StatusNotify {
    Received,
    Forwarded,
    Delivered,
    Deleted,
}

#[derive(Debug)]
pub struct Bundle {
    pub source: Eid,
    pub expiry: time::OffsetDateTime,
    pub ack_requested: bool,
    pub payload: Bytes,
}

#[async_trait]
pub trait Service: Send + Sync {
    async fn on_register(&self, source: &Eid, sink: Box<dyn Sink>);

    async fn on_unregister(&self);

    async fn on_receive(&self, bundle: Bundle);

    async fn on_status_notify(
        &self,
        bundle_id: &str,
        kind: StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<hardy_bpv7::dtn_time::DtnTime>,
    );
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendOptions {
    pub do_not_fragment: bool,
    pub request_ack: bool,
    pub report_status_time: bool,
    pub notify_reception: bool,
    pub notify_forwarding: bool,
    pub notify_delivery: bool,
    pub notify_deletion: bool,
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn unregister(&self);

    async fn send(
        &self,
        destination: Eid,
        data: &[u8],
        lifetime: std::time::Duration,
        flags: Option<SendOptions>,
    ) -> Result<Box<str>>;

    async fn cancel(&self, bundle_id: &str) -> Result<bool>;
}
