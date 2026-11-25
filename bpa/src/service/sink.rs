//! The interface a payload service sends through.

use core::time::Duration;

use hardy_bpv7::{bundle::Id, eid::Eid};

use super::{Result, Segment};
use crate::{
    Bytes, async_trait,
    stream::{Receiver, buffer_stream},
};

/// Options controlling how [`Sink::send`] builds a bundle.
///
/// Every field defaults to `false`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendOptions {
    /// Sets the "do not fragment" bundle processing flag (RFC 9171 Section
    /// 4.2.3).
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

/// The interface a [`Service`](super::Service) uses to send payloads.
///
/// The sink is supplied in the [`Context`](super::Context) the service is
/// built from.
///
/// # Lifecycle
///
/// A service takes itself out of service with
/// [`unregister`](Self::unregister): the BPA withdraws the registration
/// and calls [`Service::unregister`](super::Service::unregister). A BPA
/// shutting down calls that method itself. Either way every method here
/// returns [`Error::Disconnected`](super::Error::Disconnected)
/// afterwards.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the service from the BPA.
    async fn unregister(&self);

    /// Sends `data` to `destination`, wrapped in a bundle by the BPA.
    ///
    /// Returns the id of the bundle the BPA created.
    async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<Id>;

    /// Sends a payload read from `stream`, wrapped in a bundle by the BPA.
    ///
    /// `stream` must deliver exactly `total_len` payload bytes, since the
    /// declared length frames the bundle's wire form before the first
    /// segment is pulled, and it is read once.
    ///
    /// The provided implementation buffers the stream with
    /// [`buffer_stream`] and calls [`send`](Self::send). The BPA's own
    /// sink streams the payload end to end instead.
    ///
    /// # Errors
    ///
    /// Returns [`PayloadTooLarge`](super::Error::PayloadTooLarge) or
    /// [`PayloadUnderrun`](super::Error::PayloadUnderrun) if the stream
    /// delivers more or fewer bytes than declared,
    /// [`StreamCancelled`](super::Error::StreamCancelled) if the producer
    /// goes away before its final segment, and
    /// [`DuplicateBundle`](super::Error::DuplicateBundle) on a
    /// creation-timestamp collision, which the caller may retry.
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
