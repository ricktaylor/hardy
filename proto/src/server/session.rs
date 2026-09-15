// The session state shared by every gRPC surface.
//
// A session is one Subscribe stream: registration mints a bearer
// token, the token resolves data-plane calls to the live session (the
// live-session index in `subscribe`), and teardown is one broadcast.
// `Session` is the state a surface struct embeds.
//
// Invariants: `abort` is the one teardown entry, and the response
// stream ends itself on the trigger, so no sender is ever dropped by
// hand.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use hardy_async::CancellationToken;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::Status;

use crate::token::Token;

/// Aborts its session when dropped; see [`Session::guard`].
pub struct SessionGuard(CancellationToken);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// One live session: the state a surface struct embeds. `E` is the
/// message type of the session's Subscribe stream.
pub struct Session<E> {
    // The session-map key, minted before the struct is built.
    token: Token,
    // The teardown trigger, a child of the pool's token: pool
    // shutdown tears every session.
    cancel: CancellationToken,
    // The down direction of the Subscribe stream.
    events: Sender<Result<E, Status>>,
}

impl<E> Session<E> {
    pub fn new(token: Token, cancel: CancellationToken, events: Sender<Result<E, Status>>) -> Self {
        Self {
            token,
            cancel,
            events,
        }
    }

    /// The session's token: the key its data-plane RPCs present.
    pub fn token(&self) -> &Token {
        &self.token
    }

    /// The teardown broadcast, for doors and held work to select on.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Sends one event down the session stream, racing the send
    /// against teardown so a full buffer never outlives the session.
    /// Returns whether the session was still open to receive it; an
    /// aborted session drops the event, which is the fire-and-forget
    /// contract of the event plane.
    pub async fn event(&self, event: E) -> bool {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => false,
            sent = self.events.send(Ok(event)) => sent.is_ok(),
        }
    }

    /// Fires the teardown trigger: the one way a session ends. The
    /// session loop catches it and unregisters.
    pub fn abort(&self) {
        self.cancel.cancel();
    }

    /// A guard that aborts the session when dropped: attached to the
    /// Subscribe response stream, so tonic dropping the stream (the
    /// rpc dying for any reason) is a push onto the teardown trigger.
    pub fn guard(&self) -> SessionGuard {
        SessionGuard(self.cancel.clone())
    }

    /// The session's response stream: `registration` first, by
    /// construction ahead of any event already sitting in `events`
    /// (the receiving half of this session's channel), then the
    /// events. It drains what the session accepted, ends on abort,
    /// and aborts the session when dropped.
    pub fn stream(&self, registration: E, events: Receiver<Result<E, Status>>) -> SessionStream<E> {
        SessionStream {
            registration: Some(registration),
            events: ReceiverStream::new(events),
            cancelled: Box::pin(self.cancel.clone().cancelled_owned()),
            _guard: self.guard(),
        }
    }
}

/// The response stream of one Subscribe session; see
/// [`Session::stream`].
pub struct SessionStream<E> {
    // The stream's guaranteed first item: the wire promises the
    // Registration precedes every event, and the BPA can fire events
    // into the channel from inside `register_*` itself.
    registration: Option<E>,
    events: ReceiverStream<Result<E, Status>>,
    // Ends the stream once the session aborts, so the rpc completes
    // without anyone dropping a sender.
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    // Held for its `Drop` alone.
    _guard: SessionGuard,
}

impl<E: Unpin> Stream for SessionStream<E> {
    type Item = Result<E, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(registration) = this.registration.take() {
            return Poll::Ready(Some(Ok(registration)));
        }
        // Drain accepted events next; an aborted session accepts no
        // new ones, so this is bounded by the buffer.
        match Pin::new(&mut this.events).poll_next(cx) {
            Poll::Pending => {}
            ready => return ready,
        }
        this.cancelled.as_mut().poll(cx).map(|()| None)
    }
}

#[cfg(test)]
mod tests {
    use core::{
        future::Future,
        pin::pin,
        task::{Context, Waker},
    };

    use tokio_stream::StreamExt;

    use super::*;
    use crate::token::Token;

    #[tokio::test]
    async fn the_registration_precedes_events_then_the_stream_ends_on_abort() {
        let (events_tx, events_rx) = tokio::sync::mpsc::channel::<Result<&str, Status>>(4);
        let session = Session::new(Token::mint("ipn:1.7"), CancellationToken::new(), events_tx);

        // An event accepted before the stream is even built cannot
        // outrun the registration.
        assert!(session.event("accepted").await);
        let mut stream = session.stream("registered", events_rx);
        assert_eq!(stream.next().await.unwrap().unwrap(), "registered");

        session.abort();
        assert!(!session.event("after abort").await);

        // What the session accepted is delivered; then the stream
        // ends without anyone dropping a sender.
        assert_eq!(stream.next().await.unwrap().unwrap(), "accepted");
        assert!(
            stream.next().await.is_none(),
            "the stream must end on abort"
        );
    }

    #[tokio::test]
    async fn abort_fires_the_broadcast_and_stops_events() {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel::<Result<&str, Status>>(1);
        let session = Session::new(Token::mint("ipn:1.7"), CancellationToken::new(), events_tx);

        session.abort();
        assert!(session.cancellation().is_cancelled());

        // The biased race sees the teardown before the send, even with
        // buffer space available.
        assert!(!session.event("after abort").await);
        assert!(
            events_rx.try_recv().is_err(),
            "no event may follow an abort"
        );
    }

    #[tokio::test]
    async fn event_blocked_on_a_full_buffer_is_freed_by_teardown() {
        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<Result<&str, Status>>(1);
        let session = Session::new(Token::mint("ipn:1.7"), CancellationToken::new(), events_tx);
        assert!(session.event("fills the buffer").await);

        // A second send parks on the full buffer. Polled by hand rather
        // than spawned: a spawned task would not run until this one
        // yields, which is after the abort below, so it would take the
        // already-cancelled arm of the race and never reach the parked
        // state this test is named for.
        let mut parked = pin!(session.event("parked"));
        assert!(
            parked
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the send must park on the full buffer"
        );

        // Nothing drains the buffer, so only the teardown can free it.
        // Polled by hand here too: cancellation is synchronous, so the
        // very next poll must complete the send, and an implementation
        // that stayed parked fails this assertion instead of hanging
        // the suite.
        session.abort();
        let Poll::Ready(sent) = parked.poll(&mut Context::from_waker(Waker::noop())) else {
            panic!("teardown alone must free a parked event");
        };
        assert!(!sent);
    }
}
