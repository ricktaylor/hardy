//! The interface a whole-bundle service sends through.

use hardy_bpv7::bundle::Id;

use crate::{
    async_trait,
    service::{Result, Segment},
    stream::Receiver,
};

/// The interface a [`Service`](super::Service) uses to send bundles.
///
/// [`Sink`](crate::service::Sink) takes a destination, a payload and
/// options; this takes bundle bytes. The service builds them, and
/// the BPA parses and validates what arrives, since a service is not
/// trusted.
///
/// The sink is supplied in the [`Context`](super::Context) the service
/// is built from.
///
/// # Lifecycle
///
/// A service takes itself out of service with
/// [`unregister`](Self::unregister): the BPA withdraws the registration
/// and calls [`Service::unregister`](super::Service::unregister). A BPA
/// shutting down calls that method itself. Either way every method here
/// returns [`Error::Disconnected`](crate::service::Error::Disconnected)
/// afterwards.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the service from the BPA.
    async fn unregister(&self);

    /// Sends the bundle read from `stream`.
    ///
    /// The service writes the bundle bytes segment by segment, and the
    /// BPA parses and validates the assembled bundle. A caller holding a
    /// complete bundle in memory sends it as a one-segment stream, since
    /// `Bytes` implements [`Receiver`]: `sink.send(&mut data).await`.
    ///
    /// Returns the id of the bundle the BPA accepted.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`StreamCancelled`](crate::service::Error::StreamCancelled) if the
    /// sender is dropped before the final segment,
    /// [`InvalidBundle`](crate::service::Error::InvalidBundle) if the
    /// bytes are not a valid bundle,
    /// [`InvalidDestination`](crate::service::Error::InvalidDestination)
    /// if the bundle's source is not the service's endpoint, and
    /// [`Dropped`](crate::service::Error::Dropped) if a filter or an
    /// origination check rejects the bundle.
    async fn send(&self, stream: &mut dyn Receiver<Segment>) -> Result<Id>;
}
