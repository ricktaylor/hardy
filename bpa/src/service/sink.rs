//! The channel a payload service sends through.

use core::time::Duration;

use hardy_bpv7::{bundle::Id, eid::Eid};

use super::{Result, Segment};
use crate::{
    Bytes, async_trait,
    stream::{Receiver, buffer_stream},
};

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

/// The channel a [`Service`](super::Service) sends payloads back through.
///
/// The [`Context`](super::Context) carries it at construction, and the
/// service keeps it for its entire active lifetime.
///
/// # Lifecycle
///
/// [`unregister`](Self::unregister) is how a service takes itself out of
/// service: the BPA withdraws the registration and calls
/// [`Service::unregister`](super::Service::unregister). When the BPA
/// shuts down instead, it calls that hook itself, after which every
/// operation here returns [`Error::Disconnected`](super::Error::Disconnected).
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the associated service from the BPA.
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
    /// ([`PayloadTooLarge`](super::Error::PayloadTooLarge) /
    /// [`PayloadUnderrun`](super::Error::PayloadUnderrun)), a producer that
    /// goes away before its final segment cancels the send
    /// ([`StreamCancelled`](super::Error::StreamCancelled)), and a
    /// creation-timestamp collision surfaces as
    /// [`DuplicateBundle`](super::Error::DuplicateBundle) (vanishingly
    /// rare; the caller may resend).
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
