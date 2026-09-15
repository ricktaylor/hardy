// Per-session state used by each gRPC surface.

use std::{
    pin::{Pin, pin},
    task::{Context, Poll},
};

use hardy_async::{
    CancellationToken, DropGuard,
    sync::spin::{Mutex, Once},
};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Status, Streaming};
use tracing::warn;

use crate::{
    grammar::Unregister,
    server::{
        error::{self, Error},
        leases::{Lease, Leases, Limits},
    },
};

// One live session. `E` is the Subscribe stream's message type; `S`
// is the value registration produces.
pub struct Session<E, S> {
    leases: Leases,
    events_tx: Sender<Result<E, Status>>,
    events_rx: Mutex<Option<Receiver<Result<E, Status>>>>,
    registered: Once<S>,
}

impl<E, S> Session<E, S> {
    // `depth` is the event channel capacity, in messages.
    pub fn new(
        cancel: CancellationToken,
        label: &'static str,
        limits: Limits,
        depth: usize,
    ) -> Self {
        let (events_tx, events_rx) = channel(depth);
        Self {
            leases: Leases::new(cancel, label, limits),
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
            registered: Once::new(),
        }
    }

    // Stores the registration result. A second call is ignored.
    pub fn register(&self, registered: S) {
        self.registered.call_once(|| registered);
    }

    pub fn registered(&self) -> Option<S>
    where
        S: Clone,
    {
        self.registered.get().cloned()
    }

    pub fn leases(&self) -> &Leases {
        &self.leases
    }

    // Sends one event; fails on teardown or when the event lease
    // expires.
    pub async fn event(&self, event: E) -> error::Result<()> {
        tokio::select! {
            biased;
            _ = self.leases.cancelled() => Err(Error::SessionClosed),
            sent = self.events_tx.send(Ok(event)) => sent.map_err(|_| Error::SessionClosed),
            expired = self.leases.expired(Lease::Event) => Err(expired),
        }
    }

    pub fn abort(&self) {
        self.leases.abort();
    }

    // Reads the request stream until teardown, an unregister request,
    // or the stream ends or fails.
    pub async fn serve<R: Unregister>(&self, mut requests: Streaming<R>) {
        let mut warned = false;

        loop {
            let request = tokio::select! {
                biased;
                _ = self.leases.cancelled() => break,
                request = requests.message() => request,
            };
            match request {
                Ok(Some(request)) if request.is_unregister() => break,
                // The message may carry the session token; never
                // Debug-format it.
                Ok(Some(_)) if !warned => {
                    warned = true;
                    warn!("Ignoring unexpected message on the session stream");
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    warn!("Subscription stream failed: {e}");
                    break;
                }
            }
        }
    }

    // Opens the response stream: `registration` first, then accepted
    // events. Dropping the stream aborts the session; the stream ends
    // when its cancel guard drops. Panics if called twice.
    pub fn open(&self, registration: E) -> SessionStream<E> {
        let events_rx = self
            .events_rx
            .lock()
            .take()
            .expect("a session serves one Subscribe stream");
        let cancel = CancellationToken::new();
        SessionStream {
            registration: Some(registration),
            events_rx: ReceiverStream::new(events_rx),
            guard: Some(cancel.clone().drop_guard()),
            cancel,
            _abort: self.leases.guard(),
        }
    }
}

// The response stream of one Subscribe session.
pub struct SessionStream<E> {
    // Emitted before any event.
    registration: Option<E>,
    events_rx: ReceiverStream<Result<E, Status>>,
    // Ends the stream when `guard` drops.
    cancel: CancellationToken,
    guard: Option<DropGuard>,
    // Aborts the session when the stream is dropped.
    _abort: DropGuard,
}

impl<E> SessionStream<E> {
    // Takes the guard that ends the stream when dropped. Hold it
    // through session teardown so the stream ends last, after the
    // token and the registration are gone. Panics if called twice.
    pub fn cancel_guard(&mut self) -> DropGuard {
        self.guard
            .take()
            .expect("a session stream has one cancel guard")
    }
}

impl<E: Unpin> Stream for SessionStream<E> {
    type Item = Result<E, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(registration) = this.registration.take() {
            return Poll::Ready(Some(Ok(registration)));
        }
        // Accepted events drain before the cancel ends the stream.
        match Pin::new(&mut this.events_rx).poll_next(cx) {
            Poll::Pending => {}
            ready => return ready,
        }
        pin!(this.cancel.cancelled()).poll(cx).map(|()| None)
    }
}

#[cfg(test)]
mod tests {
    use core::{
        future::Future,
        pin::pin,
        task::{Context, Waker},
        time::Duration,
    };

    use tokio_stream::StreamExt;

    use super::*;

    #[tokio::test]
    async fn an_expired_lease_tears_the_session_down() {
        // Zero-duration leases expire immediately.
        let cancel = CancellationToken::new();
        let session = Session::<&str, ()>::new(
            cancel.clone(),
            "test",
            Limits {
                claim: Duration::ZERO,
                ack: Duration::ZERO,
                ..Limits::default()
            },
            4,
        );

        assert!(matches!(
            session.leases().expired(Lease::Claim).await,
            Error::LeaseExpired(Lease::Claim)
        ));
        assert!(
            cancel.is_cancelled(),
            "the first expired lease must tear the session down"
        );

        // A second expiry on an already-closed session must still return.
        session.leases().expired(Lease::Ack).await;
    }

    #[tokio::test]
    async fn a_session_publishes_one_registration() {
        let session =
            Session::<&str, &str>::new(CancellationToken::new(), "test", Limits::default(), 1);
        assert_eq!(
            session.registered(),
            None,
            "a door racing registration must find nothing"
        );

        session.register("the sink");
        // A second `register` must not replace the stored value.
        session.register("a second sink");
        assert_eq!(session.registered(), Some("the sink"));
    }

    #[tokio::test]
    async fn the_registration_precedes_events_then_the_stream_ends_with_the_guard() {
        let session =
            Session::<&str, ()>::new(CancellationToken::new(), "test", Limits::default(), 4);

        // An event sent before `open` must still follow the registration.
        assert!(session.event("accepted").await.is_ok());
        let mut stream = session.open("registered");
        let cancel = stream.cancel_guard();
        assert_eq!(stream.next().await.unwrap().unwrap(), "registered");

        session.abort();
        assert!(session.event("after abort").await.is_err());

        assert_eq!(stream.next().await.unwrap().unwrap(), "accepted");

        // An abort alone must not end the stream.
        {
            let mut ended = pin!(stream.next());
            assert!(
                ended
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending(),
                "an aborted session must not end its stream before it is retired"
            );
        }

        drop(cancel);
        assert!(
            stream.next().await.is_none(),
            "the stream must end with the guard"
        );
    }

    #[tokio::test]
    async fn abort_fires_the_broadcast_and_stops_events() {
        let cancel = CancellationToken::new();
        let session = Session::<&str, ()>::new(cancel.clone(), "test", Limits::default(), 1);

        session.abort();
        assert!(cancel.is_cancelled());

        assert!(session.event("after abort").await.is_err());
        let mut events_rx = session.events_rx.lock().take().unwrap();
        assert!(
            events_rx.try_recv().is_err(),
            "no event may follow an abort"
        );
    }

    #[tokio::test]
    async fn event_blocked_on_a_full_buffer_is_freed_by_teardown() {
        let session =
            Session::<&str, ()>::new(CancellationToken::new(), "test", Limits::default(), 1);
        assert!(session.event("fills the buffer").await.is_ok());

        // Polled by hand: a spawned task would only run after the
        // abort below and never reach the parked state.
        let mut parked = pin!(session.event("parked"));
        assert!(
            parked
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the send must park on the full buffer"
        );

        // Cancellation is synchronous, so the very next poll must
        // complete the send.
        session.abort();
        let Poll::Ready(sent) = parked.poll(&mut Context::from_waker(Waker::noop())) else {
            panic!("teardown alone must free a parked event");
        };
        assert!(sent.is_err());
    }

    #[tokio::test]
    async fn an_event_the_client_leaves_no_room_for_ends_on_its_lease() {
        // A zero event lease: a send with buffer room still wins the
        // biased race, so the test is deterministic without a clock.
        let cancel = CancellationToken::new();
        let session = Session::<&str, ()>::new(
            cancel.clone(),
            "test",
            Limits {
                event: Duration::ZERO,
                ..Limits::default()
            },
            1,
        );

        assert!(session.event("fills the buffer").await.is_ok());
        assert!(
            matches!(
                session.event("unread").await,
                Err(Error::LeaseExpired(Lease::Event))
            ),
            "an event the client leaves no room for must end on its lease"
        );
        assert!(
            cancel.is_cancelled(),
            "the lapsed lease must close the session behind it"
        );
    }
}
