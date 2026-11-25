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

mod factory;
mod sink;

pub use factory::{BoxFactory, Context, Factory, factory_fn};
pub use sink::Sink;

/// A service that works with whole bundles.
///
/// Where [`Service`](super::Service) receives only the payload, a raw
/// service receives the bundle bytes, and builds the bundles it sends. It
/// can parse them with [`hardy_bpv7::parse::parse`] for a structural
/// parse, then apply the [`hardy_bpv7::checks`] primitives (e.g. `verify`)
/// if it has key access.
///
/// The BPA builds the service from a [`Factory`], owns it, and drives its
/// lifecycle: [`register`](Self::register) brings it up,
/// [`deliver`](Self::deliver) hands it bundles addressed to its endpoint,
/// and [`unregister`](Self::unregister) takes it down again.
/// Traffic in the other direction goes through the [`Sink`] the
/// [`Context`] carries, which the service keeps for as long as it is
/// registered.
#[async_trait]
pub trait Service: Send + Sync {
    /// Brings the service up: spawns tasks, opens the resources it serves
    /// from.
    ///
    /// Called once, as the registration opens: at
    /// [`Bpa::start`](crate::bpa::Bpa::start) for a declared service,
    /// after storage recovery, so a delivery never races the
    /// consistency check, and at the registration call for any other.
    /// An `Err` means the service cannot run at all: the registration
    /// is undone, and the error reaches whoever asked for it.
    ///
    /// The default implementation does nothing, for a service that is
    /// live from the moment it is built.
    async fn register(&self) -> Result<()> {
        Ok(())
    }

    /// Takes the service down: stops background tasks, releases
    /// resources.
    ///
    /// Called once, when the service unregisters or the BPA shuts down.
    /// The sink answers [`Error::Disconnected`](super::Error::Disconnected)
    /// to everything by then, so nothing can be sent from here; an `Err`
    /// is logged and teardown continues regardless.
    ///
    /// The default implementation does nothing.
    async fn unregister(&self) -> Result<()> {
        Ok(())
    }

    /// Delivers a bundle to this service as a stream of segments.
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
    async fn deliver(
        &self,
        bundle_id: &Id,
        expiry: OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<()>;

    /// Reports a status report received for a bundle this service sent.
    async fn report_status(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub struct NullService;

    #[async_trait]
    impl Service for NullService {
        async fn deliver(
            &self,
            _: &Id,
            _: OffsetDateTime,
            _: u64,
            _: &mut dyn Receiver<Segment>,
        ) -> Result<()> {
            Ok(())
        }

        async fn report_status(
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
