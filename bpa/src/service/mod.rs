use core::{
    fmt::{self, Display, Formatter},
    time::Duration,
};

use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use thiserror::Error;
use time::OffsetDateTime;

use crate::{
    Bytes, async_trait,
    stream::{BufferError, Receiver, buffer_stream},
};

pub mod raw;

pub(crate) mod registry;

pub use crate::stream::Segment;

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

    /// The service id denotes the node's administrative endpoint, which is
    /// structurally registered and cannot be claimed by a service
    /// (RFC 9758 Section 5.7).
    #[error("Service id {0} identifies the administrative endpoint (RFC 9758 Section 5.7)")]
    AdministrativeEndpoint(String),

    /// No IPN node ID is configured on this BPA, so IPN services cannot register.
    #[error("There is no ipn node id configured")]
    NoIpnNodeId,

    /// No DTN node ID is configured on this BPA, so DTN services cannot register.
    #[error("There is no dtn node id configured")]
    NoDtnNodeId,

    /// The sink has been dropped or the BPA has shut down.
    #[error("The sink is disconnected")]
    Disconnected,

    /// The payload exceeds a size limit; `max` is the limit exceeded:
    ///
    /// * the BPA's `max_bundle_size`, against a bundle's declared wire size
    ///   at either originate door or an ADU send's declared `total_len`,
    ///   before a payload byte is pulled, or against the bytes a drain has
    ///   taken;
    /// * an ADU send's declared `total_len`, when its stream delivers more;
    /// * a transport-backed sink's (e.g. gRPC) pre-flight limit, instead of
    ///   letting the oversized message break the underlying stream.
    ///
    /// The fields are wire-size-domain `u64`.
    #[error("Payload too large: {size} bytes exceeds the maximum of {max} bytes")]
    PayloadTooLarge { size: u64, max: u64 },

    /// A bundle stream completed with fewer bytes than its declared
    /// `total_len`. An implementation may size buffers — or frame a
    /// transfer — from the declared length before pulling the first
    /// segment, so an under-delivering producer is rejected at the seam.
    /// The fields are wire-size-domain `u64`.
    #[error("Bundle stream delivered {size} bytes of the {expected} declared")]
    PayloadUnderrun { size: u64, expected: u64 },

    /// A stream declared a `total_len` too large to address in memory on
    /// this target: no buffer can hold it, so the stream is rejected
    /// before the first segment is pulled.
    #[error("declared length of {total_len} bytes is unaddressable on this target")]
    PayloadUnaddressable { total_len: u64 },

    /// The node ID configuration doesn't support the requested service scheme.
    #[error(transparent)]
    NodeId(#[from] crate::node_ids::Error),

    /// The bundle's destination EID is not valid for sending.
    #[error("Invalid bundle destination {0}")]
    InvalidDestination(Eid),

    /// The bundle is a fragment. Fragmentation is a forwarding-time action
    /// (RFC 9171 §5.8) — a source never emits fragments — so the raw
    /// originate door rejects a service-built fragment outright.
    #[error("Cannot originate a bundle fragment")]
    FragmentedBundle,

    /// The bundle stream was cancelled: the producer dropped its sender
    /// before delivering the final segment, so no complete bundle arrived.
    #[error("The bundle stream was cancelled before completion")]
    StreamCancelled,

    /// The bundle was dropped before it was stored — by an Originate filter,
    /// an origination gate check (an expired lifetime, an exhausted hop
    /// limit, or a failed RFC 9171 validity check), or an explicit Drop
    /// route — with an optional reason code.
    #[error("Bundle dropped{}", DroppedReason(.0))]
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

// The `Dropped` message's tail: the reason code when the drop gave one,
// nothing otherwise.
struct DroppedReason<'a>(&'a Option<ReasonCode>);

impl Display for DroppedReason<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(reason) => write!(f, " ({reason:?})"),
            None => Ok(()),
        }
    }
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

impl From<BufferError> for Error {
    fn from(e: BufferError) -> Self {
        match e {
            BufferError::Cancelled => Error::StreamCancelled,
            BufferError::TooLarge { size, max } => Error::PayloadTooLarge {
                size: size as u64,
                max: max as u64,
            },
            BufferError::Underrun { size, expected } => Error::PayloadUnderrun {
                size: size as u64,
                expected: expected as u64,
            },
            BufferError::Unaddressable { total_len } => Error::PayloadUnaddressable { total_len },
        }
    }
}

/// A service that works with bundle payloads alone.
///
/// The BPA hands a delivered bundle's payload to the service and wraps
/// sent data in a bundle of its own, so bundle structure never surfaces
/// here. This is the contract most services want; one that must see or
/// build the bundle itself implements [`raw::Service`].
///
/// # Sink Lifecycle
///
/// The service receives a [`Sink`] in [`on_register`](Self::on_register)
/// and must store it for its entire active lifetime: dropping the sink is
/// how a service asks to be disconnected, so `on_register` has to store
/// it before returning, or the BPA immediately calls
/// [`on_unregister`](Self::on_unregister).
///
/// Disconnection runs in either direction. Service-initiated, the service
/// drops its sink or calls `sink.unregister()`, and the BPA calls
/// `on_unregister`. BPA-initiated, the BPA shuts down, calls
/// `on_unregister`, and the sink stops functioning.
#[async_trait]
pub trait Service: Send + Sync {
    /// Called when the service is registered with the BPA.
    ///
    /// The `sink` must be stored for the service's entire active
    /// lifetime; dropping it triggers unregistration.
    ///
    /// `source` is the endpoint ID assigned to this service, and the source
    /// of every bundle it sends.
    async fn on_register(&self, source: &Eid, sink: Box<dyn Sink>);

    /// Called when the service is being unregistered, either because the
    /// service dropped its sink or because the BPA is shutting down.
    async fn on_unregister(&self);

    /// Called when a bundle payload is delivered to this service as a stream of segments.
    ///
    /// The implementation pulls [`Segment::Next`] items from `stream`
    /// until [`Segment::Final`] (which may carry empty bytes) completes
    /// the delivery. The source endpoint of the payload is
    /// `bundle_id.source`.
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
    /// stream with [`buffer_stream`], whose errors convert into this
    /// module's [`enum@Error`] via `?`.
    async fn on_deliver(
        &self,
        bundle_id: &Id,
        expiry: OffsetDateTime,
        ack_requested: bool,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<()>;

    /// Called when a status report is received for a bundle sent by this service.
    async fn on_status_notify(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    );
}

/// Options controlling bundle construction when sending via [`Sink::send`].
///
/// All fields default to `false`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendOptions {
    /// Sets the "do not fragment" bundle processing flag (RFC 9171 Section 4.2.3).
    pub do_not_fragment: bool,
    /// Requests an application-level acknowledgement from the destination.
    pub request_ack: bool,
    /// Includes timestamps in status reports (RFC 9171 Section 6.1.1).
    pub report_status_time: bool,
    /// Requests a "received" status report from each forwarding node.
    pub notify_reception: bool,
    /// Requests a "forwarded" status report from each forwarding node.
    pub notify_forwarding: bool,
    /// Requests a "delivered" status report when the bundle reaches its
    /// destination.
    pub notify_delivery: bool,
    /// Requests a "deleted" status report if the bundle is deleted.
    pub notify_deletion: bool,
}

/// The channel a [`Service`] sends payloads back through.
///
/// This is handed to [`Service::on_register`] and must be stored for the
/// service's entire active lifetime.
///
/// # Lifecycle
///
/// Dropping this sink is how a service unregisters: the BPA detects the
/// drop and calls [`Service::on_unregister`]. When the BPA shuts down
/// instead, it calls [`Service::on_unregister`] first, after which every
/// operation here returns [`Error::Disconnected`].
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the associated service from the BPA.
    ///
    /// Equivalent to dropping the sink, but lets the service choose when
    /// the teardown happens.
    async fn unregister(&self);

    /// Sends a payload to a destination, wrapped in a bundle by the BPA.
    async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<Id>;

    /// Sends a payload supplied as a segment stream, wrapped in a bundle
    /// by the BPA.
    ///
    /// The stream must deliver exactly `total_len` payload bytes — the
    /// declaration frames the bundle's wire form before the first segment
    /// is pulled — and the stream is one-shot: an over- or
    /// under-delivering producer is rejected
    /// ([`PayloadTooLarge`](Error::PayloadTooLarge) /
    /// [`PayloadUnderrun`](Error::PayloadUnderrun)), a producer that goes
    /// away before its final segment cancels the send
    /// ([`StreamCancelled`](Error::StreamCancelled)), and a
    /// creation-timestamp collision surfaces as
    /// [`DuplicateBundle`](Error::DuplicateBundle) (vanishingly rare; the
    /// caller may resend).
    ///
    /// The provided implementation buffers the stream
    /// ([`buffer_stream`]) and delegates to [`send`](Self::send), with the
    /// same error surface; the BPA's own sink streams end to end.
    async fn send_streamed(
        &self,
        destination: Eid,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<Id> {
        let data = buffer_stream(stream, total_len).await?;
        self.send(destination, data, lifetime, options).await
    }
}
