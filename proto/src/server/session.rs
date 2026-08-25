// The session state shared by every gRPC surface.
//
// A session is one Subscribe stream: registration mints a bearer
// token, the token resolves data-plane calls to the live session, and
// teardown is one broadcast. `Session` is the state a surface struct
// embeds; `Sessions` is the index of one bridge's live sessions.
//
// Invariants: the map is an index, never an owner; a session is
// removed only by its own session loop; `abort` is the one teardown
// entry, and the response stream ends itself on the trigger, so no
// sender is ever dropped by hand.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::CancellationToken;
use hardy_bpa::Bytes;
#[cfg(test)]
use tokio::sync::broadcast;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::Status;

use crate::server::token::{SessionToken, Signer};

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
    token: SessionToken,
    // The teardown trigger, a child of the pool's token: pool
    // shutdown tears every session.
    cancel: CancellationToken,
    // The down direction of the Subscribe stream.
    events: Sender<Result<E, Status>>,
}

impl<E> Session<E> {
    pub fn new(
        token: SessionToken,
        cancel: CancellationToken,
        events: Sender<Result<E, Status>>,
    ) -> Self {
        Self {
            token,
            cancel,
            events,
        }
    }

    /// The session's token: the key its data-plane RPCs present.
    pub fn token(&self) -> &SessionToken {
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

/// The live sessions of one bridge, each entry the surface component
/// (which embeds the [`Session`]) under its token: minted at
/// registration, resolved by every data-plane RPC in a single map
/// probe, retired at teardown. `S` is the surface's component struct.
///
/// The tokens are server-minted random values, so there is no
/// client-controlled collision-DoS vector, and the map uses the fast
/// `foldhash` hasher over the DoS-resistant default.
pub struct Sessions<S> {
    map: DashMap<SessionToken, Arc<S>, RandomState>,
    signer: Signer,
    // Fires each session's token once its teardown has fully run: the
    // race-free barrier a test waits on instead of polling for the
    // token to stop resolving. See [`torn_down`](Self::torn_down).
    #[cfg(test)]
    torn_down: broadcast::Sender<SessionToken>,
}

impl<S> Sessions<S> {
    /// An empty index minting tokens with the server-wide `signer`.
    pub fn new(signer: Signer) -> Self {
        Self {
            map: DashMap::with_hasher(RandomState::default()),
            signer,
            #[cfg(test)]
            torn_down: broadcast::channel(256).0,
        }
    }

    /// Mints a fresh token for the registration identity `sub`; only
    /// the registering task holds it until the client receives it.
    pub fn mint(&self, sub: &str) -> SessionToken {
        self.signer.mint(sub)
    }

    /// Publishes a session's component under its token.
    pub fn publish(&self, token: SessionToken, component: Arc<S>) {
        self.map.insert(token, component);
    }

    /// The component whose session a presented token authorises.
    /// Possession is the proof: a forged or retired token is simply
    /// absent from the map.
    pub fn resolve(&self, token: Bytes) -> Result<Arc<S>, Status> {
        self.map
            .get(&SessionToken::from(token))
            .map(|component| component.clone())
            .ok_or_else(|| Status::unauthenticated("Unknown session token"))
    }

    /// Retires a session; idempotent.
    pub fn remove(&self, token: &SessionToken) {
        self.map.remove(token);
    }

    /// A receiver observing every completed session teardown, by token.
    /// A test subscribes before triggering teardown, then awaits its
    /// token: once seen, the session is fully gone (the token no longer
    /// resolves and its registration is unregistered), so the next call
    /// is rejected without a race. See [`signal_torn_down`](Self::signal_torn_down).
    #[cfg(test)]
    pub fn torn_down(&self) -> broadcast::Receiver<SessionToken> {
        self.torn_down.subscribe()
    }

    /// Announces that `token`'s teardown has fully run: the last step of
    /// a surface's `unregister_session`.
    #[cfg(test)]
    pub fn signal_torn_down(&self, token: &SessionToken) {
        let _ = self.torn_down.send(token.clone());
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
    use tokio_stream::StreamExt;

    use super::*;

    #[tokio::test]
    async fn sessions_resolve_only_live_tokens() {
        let sessions = Sessions::new(Signer::new());
        let token = sessions.mint("ipn:1.7");
        sessions.publish(token.clone(), Arc::new("session"));

        let resolved = sessions.resolve(token.clone().into()).unwrap();
        assert_eq!(*resolved, "session");

        // A forged token is absent from the map.
        let forged = Bytes::from_static(b"not a token");
        assert_eq!(
            sessions.resolve(forged).unwrap_err().code(),
            tonic::Code::Unauthenticated
        );

        // Retiring is idempotent and invalidates the token.
        sessions.remove(&token);
        sessions.remove(&token);
        assert_eq!(
            sessions.resolve(token.into()).unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
    }

    #[tokio::test]
    async fn the_registration_precedes_events_then_the_stream_ends_on_abort() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<&str, Status>>(4);
        let session = Session::new(Signer::new().mint("ipn:1.7"), CancellationToken::new(), tx);

        // An event accepted before the stream is even built cannot
        // outrun the registration.
        assert!(session.event("accepted").await);
        let mut stream = session.stream("registered", rx);
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
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<&str, Status>>(1);
        let session = Session::new(Signer::new().mint("ipn:1.7"), CancellationToken::new(), tx);

        session.abort();
        assert!(session.cancellation().is_cancelled());

        // The biased race sees the teardown before the send, even with
        // buffer space available.
        assert!(!session.event("after abort").await);
        assert!(rx.try_recv().is_err(), "no event may follow an abort");
    }

    #[tokio::test]
    async fn event_blocked_on_a_full_buffer_is_freed_by_teardown() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<&str, Status>>(1);
        let session = Arc::new(Session::new(
            Signer::new().mint("ipn:1.7"),
            CancellationToken::new(),
            tx,
        ));
        assert!(session.event("fills the buffer").await);

        // A second send parks on the full buffer; abort must free it.
        let parked = tokio::spawn({
            let session = session.clone();
            async move { session.event("parked").await }
        });
        session.abort();
        let sent = tokio::time::timeout(std::time::Duration::from_secs(2), parked)
            .await
            .expect("teardown alone must free a parked event")
            .unwrap();
        assert!(!sent);
    }
}
