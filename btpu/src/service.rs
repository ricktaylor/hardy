//! Tower [`Service`] and [`Stream`] implementations for [`Sender`] and
//! [`Receiver`]. Enabled by the `tower` feature.
//!
//! - [`Receiver`] is a `Service<Bytes, Response = Vec<ReceiverEvent>>`. Each
//!   `call` processes one inbound PDU and yields the events the BPA (or other
//!   consumer) should act on.
//! - [`Sender`] is a `Service<Bytes, Response = Option<u32>>` for enqueueing
//!   bundles, and a [`Stream<Item = BytesMut>`] for draining outgoing PDUs.
//!
//! All impls are thin wrappers over the existing synchronous core; no async
//! runtime is required by this crate itself. The Service futures are
//! [`core::future::Ready`].
//!
//! # Backpressure
//!
//! Both directions of the [`Sender`] use Waker-based backpressure:
//!
//! - [`Service::poll_ready`] on [`Sender`] returns `Poll::Pending` and parks
//!   the calling task when the transfer-number window is saturated. The task
//!   wakes when a slot frees via [`Sender::complete`] or [`Sender::cancel`].
//! - [`Stream::poll_next`] on [`Sender`] returns `Poll::Pending` when the
//!   pending queue is empty (it never returns `Ready(None)` — the sender is
//!   a perpetual source until dropped). The task wakes when
//!   [`Sender::enqueue`] (via `Service::call`) pushes new messages.
//!
//! [`Receiver`]'s `Service::poll_ready` always returns `Ready(Ok(()))`; the
//! receiver has no inherent capacity limit beyond the configured window,
//! and inbound PDUs are processed synchronously inside `call`.
//!
//! # Single-owner contract
//!
//! All impls take `&mut self` (directly or via `Pin<&mut Self>`), so a
//! `Sender` or `Receiver` is owned by one task at a time. The stored wakers
//! are single-slot `Option<Waker>`s — sufficient under the single-owner
//! contract. To share a `Sender` across tasks, wrap it in `Arc<Mutex<_>>`
//! or `tower::buffer::Buffer`; the outer synchronisation serialises both
//! the state and the waker registers.

use crate::receiver::{Receiver, ReceiverEvent};
use crate::sender::Sender;
use alloc::vec::Vec;
use bytes::{Bytes, BytesMut};
use core::future::{Ready, ready};
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use tower::Service;

impl Service<Bytes> for Receiver {
    type Response = Vec<ReceiverEvent>;
    type Error = crate::receiver::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, pdu: Bytes) -> Self::Future {
        ready(self.receive_pdu(pdu))
    }
}

impl Service<Bytes> for Sender {
    type Response = Option<u32>;
    type Error = crate::sender::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.transfers_in_progress() < self.window_size() as u32 {
            Poll::Ready(Ok(()))
        } else {
            self.register_enqueue_waker(cx.waker().clone());
            Poll::Pending
        }
    }

    fn call(&mut self, bundle: Bytes) -> Self::Future {
        ready(self.enqueue(bundle))
    }
}

impl Stream for Sender {
    type Item = BytesMut;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.next_pdu() {
            Some(pdu) => Poll::Ready(Some(pdu)),
            None => {
                // The sender is a perpetual source — yielding Ready(None)
                // would mean "stream finished forever," which it isn't.
                // Park until enqueue (or cancel) pushes new messages.
                self.register_drain_waker(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
