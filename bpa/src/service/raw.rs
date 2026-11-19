//! Services that work with whole bundles.
//!
//! The raw contract suits a service that must see or build bundle
//! structure itself — an echo responder, an encapsulator, a gateway. A
//! service that only needs payloads implements
//! [`Service`](super::Service) instead.

use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use time::OffsetDateTime;

use super::{Result, Segment, StatusNotify};
use crate::{async_trait, stream::Receiver};

/// A service that works with whole bundles.
///
/// Where [`Service`](super::Service) receives only the payload, a raw
/// service receives the bundle bytes, and builds the bundles it sends. It
/// can parse them with [`hardy_bpv7::parse::parse`] for a structural
/// parse, then apply the [`hardy_bpv7::checks`] primitives (e.g. `verify`)
/// if it has key access.
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
    /// `endpoint` is the endpoint ID assigned to this service.
    async fn on_register(&self, endpoint: &Eid, sink: Box<dyn Sink>);

    /// Called when the service is being unregistered, either because the
    /// service dropped its sink or because the BPA is shutting down.
    async fn on_unregister(&self);

    /// Called when a bundle is delivered to this service as a stream of segments.
    ///
    /// The stream carries the raw bundle bytes (the service can parse them
    /// if needed); `bundle_id` is the identity of the delivered bundle and
    /// `expiry` is calculated from bundle metadata by the dispatcher. The
    /// implementation pulls [`Segment::Next`] items from `stream` until
    /// [`Segment::Final`] (which may carry empty bytes) completes the
    /// delivery.
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
    /// stream with [`buffer_stream`](crate::stream::buffer_stream), whose
    /// errors convert into the parent module's
    /// [`Error`](enum@super::Error) via `?`.
    async fn on_deliver(
        &self,
        bundle_id: &Id,
        expiry: OffsetDateTime,
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

/// The channel a [`Service`] sends whole bundles back through.
///
/// Where [`Sink`](super::Sink) takes a destination, a payload and options,
/// this takes bundle bytes: the service constructs them with
/// `bpv7::Builder`, and the BPA parses and validates what arrives — a
/// security boundary, since services are not trusted.
///
/// This is handed to [`Service::on_register`] and must be stored for the
/// service's entire active lifetime.
///
/// # Lifecycle
///
/// Dropping this sink is how a service unregisters: the BPA detects the
/// drop and calls [`Service::on_unregister`]. When the BPA shuts down
/// instead, it calls [`Service::on_unregister`] first, after which every
/// operation here returns
/// [`Error::Disconnected`](super::Error::Disconnected).
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the associated service from the BPA.
    ///
    /// Equivalent to dropping the sink, but lets the service choose when
    /// the teardown happens.
    async fn unregister(&self);

    /// Sends a bundle as a stream of [`Segment`]s.
    ///
    /// The service delivers `bpv7::Builder`-constructed bundle bytes
    /// segment by segment, and the BPA parses and validates the assembled
    /// bundle. Dropping the sender before a [`Segment::Final`] cancels the
    /// send and returns
    /// [`Error::StreamCancelled`](super::Error::StreamCancelled).
    ///
    /// A caller holding a complete bundle in memory sends it as a
    /// one-segment stream, since `Bytes` implements [`Receiver`]:
    /// `sink.send(&mut data).await`.
    async fn send(&self, stream: &mut dyn Receiver<Segment>) -> Result<Id>;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub struct NullService;

    #[async_trait]
    impl Service for NullService {
        async fn on_register(&self, _: &Eid, _: Box<dyn Sink>) {}
        async fn on_unregister(&self) {}
        async fn on_deliver(
            &self,
            _: &Id,
            _: OffsetDateTime,
            _: u64,
            _: &mut dyn Receiver<Segment>,
        ) -> Result<()> {
            Ok(())
        }
        async fn on_status_notify(
            &self,
            _: &Id,
            _: &Eid,
            _: StatusNotify,
            _: ReasonCode,
            _: Option<OffsetDateTime>,
        ) {
        }
    }
}
