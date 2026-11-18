//! The channel a whole-bundle service sends through.

use hardy_bpv7::bundle::Id;

use crate::{
    async_trait,
    service::{Result, Segment},
    stream::Receiver,
};

/// The channel a [`Service`](super::Service) sends whole bundles back
/// through.
///
/// Where [`Sink`](crate::service::Sink) takes a destination, a payload
/// and options, this takes bundle bytes: the service constructs them
/// with `bpv7::Builder`, and the BPA parses and validates what arrives —
/// a security boundary, since services are not trusted.
///
/// The [`Context`](super::Context) carries it at construction, and the
/// service keeps it for its entire active lifetime.
///
/// # Lifecycle
///
/// [`unregister`](Self::unregister) is how a service takes itself out of
/// service: the BPA withdraws the registration and calls
/// [`Service::shutdown`](super::Service::shutdown). When the BPA shuts
/// down instead, it calls `shutdown` first, after which every operation
/// here returns
/// [`Error::Disconnected`](crate::service::Error::Disconnected).
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the associated service from the BPA.
    async fn unregister(&self);

    /// Sends a bundle as a stream of [`Segment`]s.
    ///
    /// The service delivers `bpv7::Builder`-constructed bundle bytes
    /// segment by segment, and the BPA parses and validates the assembled
    /// bundle. Dropping the sender before a [`Segment::Final`] cancels the
    /// send and returns
    /// [`Error::StreamCancelled`](crate::service::Error::StreamCancelled).
    ///
    /// A caller holding a complete bundle in memory sends it as a
    /// one-segment stream, since `Bytes` implements [`Receiver`]:
    /// `sink.send(&mut data).await`.
    async fn send(&self, stream: &mut dyn Receiver<Segment>) -> Result<Id>;
}
