pub mod context;
pub mod registry;

use hardy_async::async_trait;
use hardy_bpv7::bundle::Id as BundleId;
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use thiserror::Error;

use crate::Bytes;

pub use context::ServiceContext;

/// A specialized `Result` type for service operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during service registration and bundle sending.
#[derive(Debug, Error)]
pub enum Error {
    /// The requested service id is already registered by another service.
    #[error("There is already a service registered as {0}")]
    ServiceIdInUse(String),

    /// The provided DTN service name is syntactically invalid.
    #[error("Invalid dtn service name {0}")]
    DtnInvalidServiceName(String),

    /// No IPN node ID is configured on this BPA, so IPN services cannot register.
    #[error("There is no ipn node id configured")]
    NoIpnNodeId,

    /// No DTN node ID is configured on this BPA, so DTN services cannot register.
    #[error("There is no dtn node id configured")]
    NoDtnNodeId,

    /// The context has been dropped or the BPA has shut down.
    #[error("Disconnected from BPA")]
    Disconnected,

    /// The node ID configuration doesn't support the requested service scheme.
    #[error(transparent)]
    NodeId(#[from] crate::node_ids::Error),

    /// The bundle's destination EID is not valid for sending.
    #[error("Invalid bundle destination {0}")]
    InvalidDestination(Eid),

    /// The bundle was dropped by a processing filter, with an optional reason code.
    #[error("Bundle dropped by filter: {0:?}")]
    Dropped(Option<ReasonCode>),

    /// A bundle with the same identity already exists in storage.
    #[error("Duplicate bundle already exists")]
    DuplicateBundle,

    /// The bundle failed BPv7 validation during parsing or construction.
    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

    /// An internal error from an underlying subsystem.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

/// The kind of bundle status event being reported to a service.
///
/// These correspond to the four status assertions defined in RFC 9171 Section 6.1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusNotify {
    /// The bundle was received by the reporting node.
    Received,
    /// The bundle was forwarded by the reporting node.
    Forwarded,
    /// The bundle was delivered to its destination endpoint.
    Delivered,
    /// The bundle was deleted by the reporting node.
    Deleted,
}

/// High-level application trait for services that work with payloads only.
///
/// Applications receive decoded payloads and send data that the BPA wraps in bundles.
/// This hides bundle structure details, suitable for most user services.
///
/// For services that need raw bundle access, see [`Service`].
///
/// # Context Lifecycle
///
/// The Application receives a [`ServiceContext`] in [`on_register`](Self::on_register).
/// Clone and store it if needed beyond initialization.
/// Dropping all clones closes the channels, triggering unregistration.
#[async_trait]
pub trait Application: Send + Sync {
    /// Called when the Application is registered with the BPA.
    ///
    /// # Arguments
    /// * `source` - The endpoint ID assigned to this application
    /// * `ctx` - Channel-based context for sending bundles and managing lifecycle.
    async fn on_register(&self, source: &Eid, ctx: ServiceContext);

    /// Called when the Application is being unregistered.
    ///
    /// This is called in two scenarios:
    /// 1. The Application dropped all context clones (app-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    async fn on_unregister(&self);

    /// Called when a bundle payload is delivered to this application.
    async fn on_receive(
        &self,
        source: Eid,
        expiry: time::OffsetDateTime,
        ack_requested: bool,
        payload: Bytes,
    );

    /// Called when a status report is received for a bundle sent by this application.
    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    );
}

/// Options controlling bundle construction when sending via [`ServiceContext::send`].
///
/// All fields default to `false`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendOptions {
    /// Set the "do not fragment" bundle processing flag (RFC 9171 Section 4.2.3).
    pub do_not_fragment: bool,
    /// Request an application-level acknowledgement from the destination.
    pub request_ack: bool,
    /// Include timestamps in status reports (RFC 9171 Section 6.1.1).
    pub report_status_time: bool,
    /// Request a "received" status report from each forwarding node.
    pub notify_reception: bool,
    /// Request a "forwarded" status report from each forwarding node.
    pub notify_forwarding: bool,
    /// Request a "delivered" status report when the bundle reaches its destination.
    pub notify_delivery: bool,
    /// Request a "deleted" status report if the bundle is deleted.
    pub notify_deletion: bool,
}

/// Low-level service trait with raw bundle access.
///
/// Unlike [`Application`] which receives only payload, `Service` receives
/// the raw bundle bytes.
///
/// # Context Lifecycle
///
/// The Service receives a [`ServiceContext`] in [`on_register`](Self::on_register).
/// Clone and store it if needed beyond initialization.
/// Dropping all clones closes the channels, triggering unregistration.
#[async_trait]
pub trait Service: Send + Sync {
    /// Called when service is registered with the BPA.
    ///
    /// # Arguments
    /// * `endpoint` - The endpoint ID assigned to this service
    /// * `ctx` - Channel-based context for sending bundles and managing lifecycle.
    async fn on_register(&self, endpoint: &Eid, ctx: ServiceContext);

    /// Called when the Service is being unregistered.
    ///
    /// This is called in two scenarios:
    /// 1. The Service dropped all context clones (service-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    async fn on_unregister(&self);

    /// Called when a bundle arrives
    /// - `data`: raw bundle bytes (service can parse if needed)
    /// - `expiry`: calculated from bundle metadata by dispatcher
    async fn on_receive(&self, data: Bytes, expiry: time::OffsetDateTime);

    /// Called when status report received for a sent bundle
    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub struct NullService;

    #[async_trait]
    impl Service for NullService {
        async fn on_register(&self, _: &Eid, _: ServiceContext) {}
        async fn on_unregister(&self) {}
        async fn on_receive(&self, _: Bytes, _: time::OffsetDateTime) {}
        async fn on_status_notify(
            &self,
            _: &BundleId,
            _: &Eid,
            _: StatusNotify,
            _: ReasonCode,
            _: Option<time::OffsetDateTime>,
        ) {
        }
    }
}
