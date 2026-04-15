pub mod registry;

use super::*;
use hardy_bpv7::eid::Eid;
use thiserror::Error;

/// A specialized `Result` type for service operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during service registration and bundle sending.
#[derive(Debug, Error)]
pub enum Error {
    /// The requested IPN service number is already registered by another service.
    #[error("There is already a service using ipn service number {0}")]
    IpnServiceInUse(u32),

    /// The requested DTN service demux string is already registered by another service.
    #[error("There is already a service using dtn service demux {0}")]
    DtnServiceInUse(String),

    /// The provided DTN service name is syntactically invalid.
    #[error("Invalid dtn service name {0}")]
    DtnInvalidServiceName(String),

    /// No IPN node ID is configured on this BPA, so IPN services cannot register.
    #[error("There is no ipn node id configured")]
    NoIpnNodeId,

    /// No DTN node ID is configured on this BPA, so DTN services cannot register.
    #[error("There is no dtn node id configured")]
    NoDtnNodeId,

    /// The sink has been dropped or the BPA has shut down.
    #[error("The sink is disconnected")]
    Disconnected,

    /// The bundle's destination EID is not valid for sending.
    #[error("Invalid bundle destination {0}")]
    InvalidDestination(Eid),

    /// The bundle was dropped by a processing filter, with an optional reason code.
    #[error("Bundle dropped by filter: {0:?}")]
    Dropped(Option<hardy_bpv7::status_report::ReasonCode>),

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
/// # Sink Lifecycle
///
/// The Application receives an [`ApplicationSink`] in [`on_register`](Self::on_register)
/// which it **must store** for its entire active lifetime.
///
/// **Critical**: If the Sink is dropped (either explicitly or by not storing it), the BPA
/// interprets this as the Application requesting disconnection and will call
/// [`on_unregister`](Self::on_unregister). This means `on_register` must store the Sink
/// before returning.
///
/// Two disconnection paths exist:
/// - **App-initiated**: Application drops its Sink or calls `sink.unregister()` → BPA calls `on_unregister()`
/// - **BPA-initiated**: BPA shuts down → calls `on_unregister()` → Sink becomes non-functional
#[async_trait]
pub trait Application: Send + Sync {
    /// Called when the Application is registered with the BPA.
    ///
    /// **Important**: The `sink` must be stored for the Application's entire active lifetime.
    /// Dropping the sink triggers automatic unregistration.
    ///
    /// # Arguments
    /// * `source` - The endpoint ID assigned to this application
    /// * `sink` - Communication channel back to the BPA. Must be stored.
    async fn on_register(&self, source: &Eid, sink: Box<dyn ApplicationSink>);

    /// Called when the Application is being unregistered.
    ///
    /// This is called in two scenarios:
    /// 1. The Application dropped its Sink (app-initiated disconnection)
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
        bundle_id: &hardy_bpv7::bundle::Id,
        from: &Eid,
        kind: StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    );
}

/// Options controlling bundle construction when sending via [`ApplicationSink::send`].
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

/// Sink for high-level applications to send payloads.
///
/// This is provided to [`Application::on_register`] and must be stored for the
/// Application's entire active lifetime. Dropping the Sink triggers automatic
/// unregistration.
///
/// # Lifecycle
///
/// - **App drops Sink**: BPA detects the drop and calls [`Application::on_unregister`]
/// - **BPA shuts down**: BPA calls [`Application::on_unregister`], then Sink operations return [`Error::Disconnected`]
#[async_trait]
pub trait ApplicationSink: Send + Sync {
    /// Explicitly unregisters the associated Application from the BPA.
    ///
    /// This is equivalent to dropping the Sink, but allows explicit cleanup timing.
    async fn unregister(&self);

    /// Sends a payload to a destination, wrapped in a bundle by the BPA.
    async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: core::time::Duration,
        options: Option<SendOptions>,
    ) -> Result<hardy_bpv7::bundle::Id>;

    /// Cancels transmission of a previously sent bundle. Returns `true` if the bundle was found and cancelled.
    async fn cancel(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<bool>;
}

/// Low-level service trait with raw bundle access.
///
/// Unlike [`Application`] which receives only payload, `Service` receives
/// the raw bundle bytes. This enables system services like echo that need
/// to inspect/modify bundle structure. Services can parse the bundle
/// themselves using `CheckedBundle::parse()` if they have key access.
///
/// # Sink Lifecycle
///
/// The Service receives a [`ServiceSink`] in [`on_register`](Self::on_register)
/// which it **must store** for its entire active lifetime.
///
/// **Critical**: If the Sink is dropped (either explicitly or by not storing it), the BPA
/// interprets this as the Service requesting disconnection and will call
/// [`on_unregister`](Self::on_unregister). This means `on_register` must store the Sink
/// before returning.
///
/// Two disconnection paths exist:
/// - **Service-initiated**: Service drops its Sink or calls `sink.unregister()` → BPA calls `on_unregister()`
/// - **BPA-initiated**: BPA shuts down → calls `on_unregister()` → Sink becomes non-functional
///
/// # Example
///
/// ```ignore
/// struct MyService {
///     sink: Once<Box<dyn ServiceSink>>,
/// }
///
/// impl Service for MyService {
///     async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
///         self.sink.call_once(|| sink);  // Store it
///     }
///     // ...
/// }
/// ```
#[async_trait]
pub trait Service: Send + Sync {
    /// Called when service is registered with the BPA.
    ///
    /// **Important**: The `sink` must be stored for the Service's entire active lifetime.
    /// Dropping the sink triggers automatic unregistration.
    ///
    /// # Arguments
    /// * `endpoint` - The endpoint ID assigned to this service
    /// * `sink` - Communication channel back to the BPA. Must be stored.
    async fn on_register(&self, endpoint: &Eid, sink: Box<dyn ServiceSink>);

    /// Called when the Service is being unregistered.
    ///
    /// This is called in two scenarios:
    /// 1. The Service dropped its Sink (service-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    async fn on_unregister(&self);

    /// Called when a bundle arrives
    /// - `data`: raw bundle bytes (service can parse if needed)
    /// - `expiry`: calculated from bundle metadata by dispatcher
    async fn on_receive(&self, data: Bytes, expiry: time::OffsetDateTime);

    /// Called when status report received for a sent bundle
    async fn on_status_notify(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        from: &Eid,
        kind: StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    );
}

/// Sink for low-level services to send raw bundles.
///
/// Unlike [`ApplicationSink`] which takes destination/payload/options,
/// `ServiceSink` accepts raw bundle bytes. The service uses `bpv7::Builder`
/// to construct bundles; BPA parses and validates (security boundary).
///
/// This is provided to [`Service::on_register`] and must be stored for the
/// Service's entire active lifetime. Dropping the Sink triggers automatic
/// unregistration.
///
/// # Lifecycle
///
/// - **Service drops Sink**: BPA detects the drop and calls [`Service::on_unregister`]
/// - **BPA shuts down**: BPA calls [`Service::on_unregister`], then Sink operations return [`Error::Disconnected`]
#[async_trait]
pub trait ServiceSink: Send + Sync {
    /// Explicitly unregisters the associated Service from the BPA.
    ///
    /// This is equivalent to dropping the Sink, but allows explicit cleanup timing.
    async fn unregister(&self);

    /// Sends a bundle as raw bytes.
    ///
    /// The service constructs the bundle using `bpv7::Builder`. The BPA parses
    /// and validates the bundle (security boundary - services are not trusted).
    async fn send(&self, data: Bytes) -> Result<hardy_bpv7::bundle::Id>;

    /// Cancels a pending bundle that hasn't been forwarded yet.
    async fn cancel(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<bool>;
}
