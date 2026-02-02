pub mod registry;

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

    #[error("Invalid dtn service name {0}")]
    DtnInvalidServiceName(String),

    #[error("There is no ipn node id configured")]
    NoIpnNodeId,

    #[error("There is no dtn node id configured")]
    NoDtnNodeId,

    #[error("The sink is disconnected")]
    Disconnected,

    #[error("Invalid bundle destination {0}")]
    InvalidDestination(Eid),

    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug)]
pub enum StatusNotify {
    Received,
    Forwarded,
    Delivered,
    Deleted,
}

#[async_trait]
pub trait Application: Send + Sync {
    async fn on_register(&self, source: &Eid, sink: Box<dyn ApplicationSink>);

    async fn on_unregister(&self);

    async fn on_receive(
        &self,
        source: Eid,
        expiry: time::OffsetDateTime,
        ack_requested: bool,
        payload: Bytes,
    );

    async fn on_status_notify(
        &self,
        bundle_id: &str,
        from: &str,
        kind: StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
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
pub trait ApplicationSink: Send + Sync {
    async fn unregister(&self);

    async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: std::time::Duration,
        options: Option<SendOptions>,
    ) -> Result<Box<str>>;

    async fn cancel(&self, bundle_id: &str) -> Result<bool>;
}

/// Low-level service trait with full bundle access.
///
/// Unlike `Application` which receives only payload, `Service` receives
/// the full parsed bundle and raw bytes. This enables system services
/// like echo that need to inspect/modify bundle structure.
#[async_trait]
pub trait Service: Send + Sync {
    /// Called when service is registered; receives Sink for sending
    async fn on_register(&self, endpoint: &Eid, sink: Box<dyn ServiceSink>);

    /// Called when service is unregistered
    async fn on_unregister(&self);

    /// Called when a bundle arrives
    /// - `bundle`: parsed view (BPA already parsed for routing/validation)
    /// - `data`: raw bytes (for block unpacking)
    /// - `expiry`: calculated from metadata by dispatcher
    async fn on_bundle(
        &self,
        bundle: &hardy_bpv7::bundle::Bundle,
        data: Bytes,
        expiry: time::OffsetDateTime,
    );

    /// Called when status report received for a sent bundle
    async fn on_status_notify(
        &self,
        bundle_id: &str,
        from: &str,
        kind: StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    );
}

/// Sink for low-level services to send bundles.
///
/// Unlike `ApplicationSink` which takes destination/payload/options,
/// `ServiceSink` accepts raw bundle bytes. The service uses `bpv7::Builder`
/// to construct bundles; BPA parses and validates (security boundary).
#[async_trait]
pub trait ServiceSink: Send + Sync {
    /// Unregister the service
    async fn unregister(&self);

    /// Send a bundle as raw bytes
    /// - Service uses bpv7::Builder to construct
    /// - BPA parses and validates (security boundary - can't trust service)
    async fn send(&self, data: Bytes) -> Result<hardy_bpv7::bundle::Id>;

    /// Cancel a pending bundle
    async fn cancel(&self, bundle_id: &str) -> Result<bool>;
}
