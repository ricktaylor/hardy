//! Tower [`Service`] and [`Stream`] implementations for [`Sender`] and
//! [`Receiver`]. Enabled by the `tower` feature.
//!
//! - [`Receiver`] is a `Service<Bytes, Response = Vec<ReceiverEvent>>` with
//!   `Error = Infallible`. Each `call` processes one inbound PDU and yields
//!   the events the BPA (or other consumer) should act on; decode and
//!   semantic faults are themselves events, never service errors.
//! - [`Sender`] is a `Service<SendRequest, Response = BundleTransferId>`
//!   for enqueueing bundles (`SendRequest: From<Bytes>` covers the
//!   default-options case), plus a [`Stream<Item = Pdu>`] for draining
//!   outgoing PDUs with the bundles each carries.
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
//!   the calling task when the transfer window is saturated or the send
//!   queue is at its configured depth.  The window gate applies to every
//!   request, unsegmented bundles included, because `poll_ready` cannot see
//!   the request; it never deadlocks, since a full window means Transfer End
//!   messages are queued and draining the `Stream` frees it.  The task wakes
//!   when [`Stream::poll_next`] drains a PDU or [`Sender::cancel`] frees a
//!   slot or a queue entry.  Every parked task is woken, so several producers may share one
//!   `Sender` through a mutex.
//! - [`Stream::poll_next`] on [`Sender`] returns `Poll::Pending` when the
//!   pending queue is empty (it never returns `Ready(None)`: the sender is
//!   a perpetual source until dropped). The task wakes when
//!   [`Sender::enqueue`] (via `Service::call`) pushes new messages.
//!
//! Only draining the `Stream` frees a full window or queue, so the drain
//! must be able to run while a producer is parked.  A single task that
//! awaits `ready()` before polling the `Stream` parks itself for good once
//! the window fills; drive the two halves from separate tasks, or from one
//! task that selects over both.
//!
//! [`Receiver`]'s `Service::poll_ready` always returns `Ready(Ok(()))`; the
//! receiver has no inherent capacity limit beyond the configured window,
//! and inbound PDUs are processed synchronously inside `call`.
//!
//! # Single-owner contract
//!
//! All impls take `&mut self` (directly or via `Pin<&mut Self>`), so a
//! `Sender` or `Receiver` is owned by one task at a time. To share a
//! `Sender` across tasks, wrap it in `Arc<Mutex<_>>`. Do **not** use
//! `tower::buffer::Buffer` for this: it moves the `Sender` into a worker
//! task and exposes only the `Service` half, so the `Stream` drain and
//! `cancel` become unreachable, PDUs never leave, and window slots never
//! free.
//!
//! `poll_ready` reserves nothing.  It reports whether a request would be
//! admitted at that instant, and since every producer polls the one shared
//! `Sender`, another producer's `call` can take the capacity in between.
//! Two rules make the shared case sound:
//!
//! - Make `poll_ready` and `call` one critical section: take the lock, poll,
//!   and if `Ready` call before releasing it.  A tower combinator that
//!   awaits `ready()` and then calls does not do this across a shared
//!   mutex, and its `call` can fail with
//!   [`transfer::Error::WindowFull`](crate::transfer::Error::WindowFull)
//!   after a `Ready`; treat that as "retry from `poll_ready`", not as a
//!   failed bundle.
//! - Never hold the lock across a `Pending`.  Release it before parking on
//!   the registered waker, or the drain task cannot take the lock to pack
//!   the Transfer End that would free the slot, and nothing ever wakes.
//!
//! The `poll_fn` shape, lock then poll then either call or drop the guard
//! and return `Pending`, satisfies both.

use alloc::vec::Vec;
use core::{
    convert::Infallible,
    future::{Ready, ready},
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_core::Stream;
use tower::Service;

use crate::{
    receiver::{Receiver, ReceiverEvent},
    // Aliased: distinguishes it from the receiver's and codec's `Error`s.
    sender::{BundleTransferId, Error as SenderError, Pdu, SendRequest, Sender},
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
    type Response = BundleTransferId;
    type Error = SenderError;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Two admission gates: transfer-window capacity (segmented bundles
        // allocate a number in `call`) and send-queue capacity.  The queue
        // gate is what bounds unsegmented bundles, which never take a
        // window slot.
        if self.is_window_available() && !self.is_send_queue_full() {
            Poll::Ready(Ok(()))
        } else {
            self.register_enqueue_waker(cx.waker());
            Poll::Pending
        }
    }

    fn call(&mut self, request: SendRequest) -> Self::Future {
        ready(self.enqueue(request.data, request.options))
    }
}

impl Stream for Sender {
    type Item = Pdu;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.next_pdu() {
            Some(pdu) => Poll::Ready(Some(pdu)),
            None => {
                // The sender is a perpetual source: yielding Ready(None)
                // would mean "stream finished forever," which it isn't.
                // Park until enqueue pushes new messages.
                self.register_drain_waker(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
