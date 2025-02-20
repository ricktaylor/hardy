use super::*;
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

    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug)]
pub enum ServiceName<'a> {
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

#[async_trait]
pub trait Service: Send + Sync {
    async fn on_connect(&self, sink: Box<dyn Sink>, eid: &bpv7::Eid) -> Result<()>;

    async fn on_disconnect(&self);

    async fn on_received(&self, bundle_id: &bpv7::BundleId, expiry: time::OffsetDateTime);

    async fn on_status_notify(
        &self,
        bundle_id: &bpv7::BundleId,
        kind: StatusNotify,
        reason: bpv7::StatusReportReasonCode,
        timestamp: Option<bpv7::DtnTime>,
    );
}

#[derive(Debug, Default)]
pub struct SendFlags {
    pub do_not_fragment: bool,
    pub request_ack: bool,
    pub report_status_time: bool,
    pub notify_reception: bool,
    pub notify_forwarding: bool,
    pub notify_delivery: bool,
    pub notify_deletion: bool,
}

#[derive(Debug)]
pub struct Bundle {
    pub id: bpv7::BundleId,
    pub expiry: time::OffsetDateTime,
    pub ack_requested: bool,
    pub payload: Box<[u8]>,
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn disconnect(&self);

    async fn send(
        &self,
        destination: bpv7::Eid,
        data: &[u8],
        lifetime: time::Duration,
        flags: Option<SendFlags>,
    ) -> Result<bpv7::BundleId>;

    async fn collect(&self, bundle_id: &bpv7::BundleId) -> Result<Option<Bundle>>;
}
