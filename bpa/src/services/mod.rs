pub(crate) mod registry;

use core::time::Duration;

use hardy_async::async_trait;
use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use thiserror::Error;

use crate::Bytes;

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

    /// The sink has been dropped or the BPA has shut down.
    #[error("The sink is disconnected")]
    Disconnected,

    /// The payload exceeds the transport's maximum message size and was
    /// rejected before being sent. Returned by transport-backed sinks
    /// (e.g. gRPC) when a pre-flight size check fails, instead of
    /// letting the oversized message break the underlying stream.
    #[error("Payload too large: {size} bytes exceeds the maximum of {max} bytes")]
    PayloadTooLarge { size: usize, max: usize },

    /// A bundle stream completed with fewer bytes than its declared
    /// `total_len`. An implementation may size buffers — or frame a
    /// transfer — from the declared length before pulling the first
    /// segment, so an under-delivering producer is rejected at the seam.
    #[error("Bundle stream delivered {size} bytes of the {expected} declared")]
    PayloadUnderrun { size: usize, expected: usize },

    #[error("declared length of {total_len} bytes is unaddressable on this target")]
    PayloadUnaddressable { total_len: u64 },

    /// The node ID configuration doesn't support the requested service scheme.
    #[error(transparent)]
    NodeId(#[from] crate::node_ids::Error),

    /// The bundle's destination EID is not valid for sending.
    #[error("Invalid bundle destination {0}")]
    InvalidDestination(Eid),

    /// The bundle stream was cancelled: the producer dropped its sender
    /// before delivering the final segment, so no complete bundle arrived.
    #[error("The bundle stream was cancelled before completion")]
    StreamCancelled,

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

impl From<crate::stream::BufferError> for Error {
    fn from(e: crate::stream::BufferError) -> Self {
        match e {
            crate::stream::BufferError::Cancelled => Error::StreamCancelled,
            crate::stream::BufferError::TooLarge { size, max } => {
                Error::PayloadTooLarge { size, max }
            }
            crate::stream::BufferError::Underrun { size, expected } => {
                Error::PayloadUnderrun { size, expected }
            }
            crate::stream::BufferError::Unaddressable { total_len } => {
                Error::PayloadUnaddressable { total_len }
            }
        }
    }
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

    /// Called when a bundle payload is delivered to this application as a stream of segments.
    ///
    /// The implementation pulls
    /// [`Segment::Next`](crate::stream::Segment::Next) items from `stream`
    /// until [`Segment::Final`](crate::stream::Segment::Final) (which may
    /// carry empty bytes) completes the delivery. The source endpoint of
    /// the payload is `bundle_id.source`.
    ///
    /// A pull returning `Err(`[`RecvError`](crate::stream::RecvError)`)`
    /// before `Final` means the producer aborted the delivery: the
    /// implementation must not act on the partial payload and must not
    /// return `Ok`. Returning `Err` parks the bundle as
    /// `WaitingForService`; a subsequent registration on the same EID
    /// re-delivers it.
    ///
    /// `total_len` is the exact number of payload bytes the stream will
    /// deliver — the sum of all segment payload lengths — carried as `u64`
    /// so it remains valid on 32-bit targets. An implementation may size
    /// buffers from it before pulling the first segment.
    ///
    /// An implementation that needs the whole payload in memory buffers the
    /// stream with [`stream::buffer_stream`](crate::stream::buffer_stream),
    /// whose errors convert into this module's [`Error`] via `?`.
    async fn on_deliver(
        &self,
        bundle_id: &Id,
        expiry: time::OffsetDateTime,
        ack_requested: bool,
        total_len: u64,
        stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
    ) -> Result<()>;

    /// Called when a status report is received for a bundle sent by this application.
    async fn on_status_notify(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
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
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<Id>;
}

/// Low-level service trait with raw bundle access.
///
/// Unlike [`Application`] which receives only payload, `Service` receives
/// the raw bundle bytes. This enables system services like echo that need
/// to inspect/modify bundle structure. Services can parse the bundle
/// themselves with [`hardy_bpv7::parse::parse`] for a structural parse, then
/// apply the [`hardy_bpv7::checks`] primitives (e.g. `verify`) if they have
/// key access.
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

    /// Called when a bundle is delivered to this service as a stream of segments.
    ///
    /// The stream carries the raw bundle bytes (the service can parse them
    /// if needed); `bundle_id` is the identity of the delivered bundle and
    /// `expiry` is calculated from bundle metadata by the dispatcher. The
    /// implementation pulls
    /// [`Segment::Next`](crate::stream::Segment::Next) items from `stream`
    /// until [`Segment::Final`](crate::stream::Segment::Final) (which may
    /// carry empty bytes) completes the delivery.
    ///
    /// A pull returning `Err(`[`RecvError`](crate::stream::RecvError)`)`
    /// before `Final` means the producer aborted the delivery: the
    /// implementation must not act on the partial bundle and must not
    /// return `Ok`. Returning `Err` parks the bundle as
    /// `WaitingForService`; a subsequent registration on the same EID
    /// re-delivers it.
    ///
    /// `total_len` is the exact number of bundle bytes the stream will
    /// deliver — the sum of all segment payload lengths — carried as `u64`
    /// so it remains valid on 32-bit targets. An implementation may size
    /// buffers from it before pulling the first segment.
    ///
    /// An implementation that needs the whole bundle in memory buffers the
    /// stream with [`stream::buffer_stream`](crate::stream::buffer_stream),
    /// whose errors convert into this module's [`Error`] via `?`.
    async fn on_deliver(
        &self,
        bundle_id: &Id,
        expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
    ) -> Result<()>;

    /// Called when status report received for a sent bundle
    async fn on_status_notify(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
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

    /// Sends a bundle as a stream of [`Segment`](crate::stream::Segment)s.
    ///
    /// The service delivers `bpv7::Builder`-constructed bundle bytes segment
    /// by segment, and the BPA parses and validates the assembled bundle
    /// (security boundary - services are not trusted). Dropping the sender
    /// before a [`Segment::Final`](crate::stream::Segment::Final) cancels
    /// the send and returns [`Error::StreamCancelled`].
    ///
    /// A caller holding a complete bundle in memory sends it as a
    /// one-segment stream, since `Bytes` implements [`stream::Receiver`](crate::stream::Receiver):
    /// `sink.send(&mut data).await`.
    async fn send(
        &self,
        stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
    ) -> Result<Id>;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub struct NullService;

    #[async_trait]
    impl Service for NullService {
        async fn on_register(&self, _: &Eid, _: Box<dyn ServiceSink>) {}
        async fn on_unregister(&self) {}
        async fn on_deliver(
            &self,
            _: &Id,
            _: time::OffsetDateTime,
            _: u64,
            _: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
        ) -> Result<()> {
            Ok(())
        }
        async fn on_status_notify(
            &self,
            _: &Id,
            _: &Eid,
            _: StatusNotify,
            _: ReasonCode,
            _: Option<time::OffsetDateTime>,
        ) {
        }
    }
}
