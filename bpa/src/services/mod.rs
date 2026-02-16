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

    #[error("Bundle dropped by filter: {0:?}")]
    Dropped(Option<hardy_bpv7::status_report::ReasonCode>),

    #[error("Duplicate bundle already exists")]
    DuplicateBundle,

    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusNotify {
    Received,
    Forwarded,
    Delivered,
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

    async fn on_receive(
        &self,
        source: Eid,
        expiry: time::OffsetDateTime,
        ack_requested: bool,
        payload: Bytes,
    );

    async fn on_status_notify(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        from: &Eid,
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
