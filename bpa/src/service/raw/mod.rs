//! Services that work with whole bundles.
//!
//! A raw service sees and builds bundle structure itself, as an echo
//! responder, an encapsulator or a gateway does. A service that needs
//! only payloads implements [`Service`](super::Service) instead.

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
/// [`Service`](super::Service) receives payloads; a raw service receives
/// bundle bytes and builds the bundles it sends. It can parse
/// them with [`hardy_bpv7::parse::parse`], then apply the
/// [`hardy_bpv7::checks`] primitives if it has key access.
///
/// The BPA builds the service from a [`Factory`], owns it, and drives
/// its lifecycle: [`register`](Self::register) brings it up,
/// [`deliver`](Self::deliver) hands it bundles addressed to its
/// endpoint, and [`unregister`](Self::unregister) takes it down. The
/// service sends through the [`Sink`] in the [`Context`] it was built
/// from.
#[async_trait]
pub trait Service: Send + Sync {
    /// Brings the service up, spawning tasks and opening the resources
    /// it serves from.
    ///
    /// Called once, when the registration opens. For a service declared
    /// on the [`BpaBuilder`](crate::builder::BpaBuilder) that is during
    /// [`Bpa::start`](crate::bpa::Bpa::start), after storage recovery, so
    /// a delivery never races the consistency check.
    ///
    /// The default implementation does nothing, which suits a service
    /// that is live as soon as it is built.
    ///
    /// # Errors
    ///
    /// An error means the service cannot run at all. The registration is
    /// undone and the error is returned to the caller that asked for it.
    async fn register(&self) -> Result<()> {
        Ok(())
    }

    /// Takes the service down, stopping background tasks and releasing
    /// resources.
    ///
    /// Called once, when the service unregisters or the BPA shuts down.
    /// The sink already answers
    /// [`Error::Disconnected`](super::Error::Disconnected), so nothing
    /// can be sent from here.
    ///
    /// The default implementation does nothing.
    ///
    /// # Errors
    ///
    /// An error is logged, and teardown continues.
    async fn unregister(&self) -> Result<()> {
        Ok(())
    }

    /// Delivers a bundle to this service as a stream of segments.
    ///
    /// The stream carries the raw bundle bytes. `bundle_id` identifies
    /// the bundle, and `expiry` is calculated by the dispatcher from the
    /// bundle metadata. The implementation pulls [`Segment::Next`] items
    /// from `stream` until a [`Segment::Final`], whose bytes may be
    /// empty, completes the delivery.
    ///
    /// `total_len` is the exact number of bundle bytes the stream
    /// delivers, the sum of all segment payload lengths. It is a `u64`,
    /// so it remains valid on 32-bit targets, and an implementation may
    /// size buffers from it before pulling the first segment. An
    /// implementation that needs the whole bundle in memory buffers the
    /// stream with [`buffer_stream`](crate::stream::buffer_stream), whose
    /// errors convert into [`Error`](enum@super::Error) with `?`.
    ///
    /// # Errors
    ///
    /// A pull returning [`RecvError`](crate::stream::RecvError) before
    /// the final segment means the producer abandoned the delivery: the
    /// implementation must not act on the partial bundle, and must return
    /// an error rather than `Ok`. An error parks the bundle as
    /// `WaitingForService`, and a later registration on the same endpoint
    /// re-delivers it.
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
