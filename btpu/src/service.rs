//! Tower [`Service`] and [`Stream`] implementations for [`Sender`] and
//! [`Receiver`], enabled by the `tower` feature: thin wrappers over the
//! synchronous core, whose futures are [`core::future::Ready`].  The
//! backpressure and sharing contract is documented on [`Sender`] under
//! Concurrency; [`Receiver`]'s service is always ready and never fails.

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
    sender::{Error as SenderError, Pdu, SendId, SendRequest, Sender},
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
    type Response = SendId;
    type Error = SenderError;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Two admission gates: transfer-window capacity (segmented bundles
        // allocate a number in `call`) and the send queue's high
        // watermark.  The watermark is what holds back unsegmented
        // bundles, which never take a window slot.
        if self.is_window_available() && !self.is_send_queue_high() {
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
                // Park until enqueue or push supplies more.
                self.register_drain_waker(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
