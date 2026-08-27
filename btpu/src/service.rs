//! Tower [`Service`] and [`Stream`] implementations for [`Sender`] and
//! [`Receiver`]. Enabled by the `tower` feature.
//!
//! - [`Receiver`] is a `Service<Bytes, Response = Vec<ReceiverEvent>>` with
//!   `Error = Infallible`. Each `call` processes one inbound PDU and yields
//!   the events the BPA (or other consumer) should act on; decode and
//!   semantic faults are themselves events, never service errors.
//! - [`Sender`] is a `Service<SendRequest, Response = Enqueued>` for
//!   enqueueing bundles with [`SendOpts`] (and a `Service<Bytes>`
//!   default-options convenience), plus a [`Stream<Item = BytesMut>`] for
//!   draining outgoing PDUs.
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
//!   the calling task when the transfer-number window is saturated or the
//!   send queue is at its configured depth — the latter is what bounds
//!   unsegmented bundles, which never take a window slot. The task wakes
//!   when a slot frees via [`Sender::complete`] / [`Sender::cancel`] or when
//!   [`Sender::next_pdu`] (via `Stream::poll_next`) drains the queue.
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
//! contract. To share a `Sender` across tasks, wrap it in `Arc<Mutex<_>>`;
//! the outer synchronisation serialises both the state and the waker
//! registers. Do **not** use `tower::buffer::Buffer` for this: it moves the
//! `Sender` into a worker task and exposes only the `Service` half, so the
//! `Stream` drain and `complete`/`cancel` become unreachable — PDUs never
//! leave and window slots never free.

use alloc::vec::Vec;
use core::{
    convert::Infallible,
    future::{Ready, ready},
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use tower::Service;

use crate::{
    receiver::{Receiver, ReceiverEvent},
    sender::{Enqueued, SendOpts, SendRequest, Sender},
};

impl Service<Bytes> for Receiver {
    type Response = Vec<ReceiverEvent>;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, pdu: Bytes) -> Self::Future {
        ready(Ok(self.receive_pdu(pdu)))
    }
}

impl Service<SendRequest> for Sender {
    type Response = Enqueued;
    type Error = crate::sender::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Two admission gates: transfer-window capacity (segmented bundles
        // allocate a number in `call`) and send-queue capacity.  The queue
        // gate is what bounds unsegmented bundles, which never take a
        // window slot.
        if self.window_available() && !self.send_queue_full() {
            Poll::Ready(Ok(()))
        } else {
            self.register_enqueue_waker(cx.waker().clone());
            Poll::Pending
        }
    }

    fn call(&mut self, request: SendRequest) -> Self::Future {
        ready(self.enqueue(request.data, request.opts))
    }
}

/// Default-options convenience over [`Service<SendRequest>`].
impl Service<Bytes> for Sender {
    type Response = Enqueued;
    type Error = crate::sender::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <Self as Service<SendRequest>>::poll_ready(self, cx)
    }

    fn call(&mut self, bundle: Bytes) -> Self::Future {
        ready(self.enqueue(bundle, SendOpts::default()))
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
