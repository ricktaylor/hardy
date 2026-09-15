// The Subscribe rpc, served for every surface by one workflow.
//
// A client subscribes; the server handles that subscription for as long
// as it lasts. [`SubscribeHandler`] is what a surface must say about its
// own wire to be handled: how to register with the BPA, what each client
// message means, and how to unregister. [`Sessions`] is every
// subscription a surface is currently serving, indexed by the token its
// data-plane doors present. The workflow itself, `Sessions::serve`, is
// written once here.
//
// The workflow runs on a pool task, not in the rpc handler, and that is
// load-bearing: tonic drops a handler future the moment its rpc dies,
// and the BPA publishes a registration part-way through `register_*`,
// so a handler registering inline can be dropped mid-commit and leave
// the BPA holding a component nothing can unregister. A task cannot be
// cancelled by the client, so the registration always completes and the
// workflow then asks whether anyone is still there to receive the
// stream, unregistering on the spot if not. The rpc handler is a proxy
// that spawns the task and awaits its answer through a one-shot.

use core::{future::Future, ops::ControlFlow};
use std::sync::Arc;

use dashmap::DashMap;
use foldhash::fast::RandomState;
use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::{Bytes, bpa::BpaRegistration};
#[cfg(test)]
use tokio::sync::broadcast;
use tokio::sync::{mpsc, oneshot};
use tonic::{Response, Status, Streaming};
use tracing::warn;
#[cfg(feature = "instrument")]
use tracing::{Instrument, Span, trace_span};

use crate::{
    server::{
        CHANNEL_DEPTH,
        session::{Session, SessionStream},
    },
    token::Token,
};

/// One gRPC surface's side of the Subscribe rpc: everything the shared
/// workflow cannot know. The surface type that implements it is the
/// component the BPA registers, so it is also what the data-plane doors
/// resolve their token to.
pub trait SubscribeHandler: Send + Sync + Sized + 'static {
    /// The event message pushed down the subscription.
    type Event: Send + Unpin + 'static;

    /// The request message the client sends up the subscription.
    type Request: Send + 'static;

    /// The payload of the client's opening `Register`.
    type Register: Send;

    /// Names the surface in tracing spans and session tokens.
    const LABEL: &'static str;

    /// The subscription's event buffer, in messages. Events are small,
    /// so this only smooths bursts.
    const EVENT_DEPTH: usize = CHANNEL_DEPTH;

    /// The session state this handler embeds.
    fn session(&self) -> &Session<Self::Event>;

    /// The `Register` payload, if this is the client's opening message.
    /// `None` for anything else, which fails the subscription.
    fn into_register(request: Self::Request) -> Option<Self::Register>;

    /// Mints the session token, builds the component around
    /// `Session::new(token, cancel, events_tx)`, and registers it with
    /// the BPA. Returns the component and the `Registration` event the
    /// subscription leads with, or the status the rpc fails with.
    ///
    /// Whatever this commits, the workflow unregisters: it is called
    /// from a task that always runs to its end, so a registration is
    /// never abandoned part-way.
    fn register(
        bpa: &Arc<dyn BpaRegistration>,
        register: Self::Register,
        cancel: CancellationToken,
        events_tx: mpsc::Sender<Result<Self::Event, Status>>,
    ) -> impl Future<Output = Result<(Arc<Self>, Self::Event), Status>> + Send;

    /// What one client message means. The workflow owns the endings
    /// every surface shares (half-close, a failed stream, teardown), so
    /// this answers only for messages: [`ControlFlow::Break`] ends the
    /// subscription, and the workflow runs the one teardown.
    fn on_request(&self, request: Self::Request) -> impl Future<Output = ControlFlow<()>> + Send;

    /// Unregisters from the BPA. The sink type is surface-specific, so
    /// the call is the handler's; a repeat unregister no-ops inside the
    /// sink, and a component that never registered one does nothing.
    fn unregister(&self) -> impl Future<Output = ()> + Send;
}

/// Every subscription one surface is currently serving: the BPA the
/// registrations are made against, the pool their tasks run on, and the
/// live-session index.
///
/// The index holds each handler (which embeds its [`Session`]) under its
/// token, minted at registration, resolved by every data-plane rpc in a
/// single map probe, retired at teardown. It is an index, never an
/// owner; an entry is only ever removed by the workflow that put it
/// there. Shutting down the pool tears the subscriptions and drives
/// unregistration, so shut it down only after the transport has stopped
/// accepting.
///
/// The tokens are server-minted random values, so there is no
/// client-controlled collision-DoS vector, and the map uses the fast
/// `foldhash` hasher over the DoS-resistant default.
pub struct Sessions<S: SubscribeHandler> {
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    sessions: Arc<DashMap<Token, Arc<S>, RandomState>>,
    // Fires each session's token once its teardown has fully run, so a
    // test has a barrier instead of polling. See
    // [`torn_down`](Self::torn_down).
    #[cfg(test)]
    torn_down: broadcast::Sender<Token>,
}

impl<S: SubscribeHandler> Sessions<S> {
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            bpa,
            tasks,
            sessions: Arc::new(DashMap::with_hasher(RandomState::default())),
            #[cfg(test)]
            torn_down: broadcast::channel(256).0,
        }
    }

    /// Serves one Subscribe call: the whole of a surface's `subscribe`
    /// handler. The workflow runs on the pool, so the rpc's own return
    /// value comes back through a one-shot; a receiver already dropped
    /// is how the workflow learns the rpc did not survive its own
    /// registration.
    pub async fn subscribe(
        &self,
        requests: Streaming<S::Request>,
    ) -> Result<Response<SessionStream<S::Event>>, Status> {
        let (response_tx, response_rx) = oneshot::channel();

        let task = self.clone().serve(requests, response_tx);
        #[cfg(feature = "instrument")]
        {
            let span = trace_span!(parent: None, "grpc_session", surface = S::LABEL);
            span.follows_from(Span::current());
            self.tasks.spawn(task.instrument(span));
        }
        #[cfg(not(feature = "instrument"))]
        self.tasks.spawn(task);

        response_rx
            .await
            .unwrap_or_else(|_| Err(Status::unavailable("Shutting down")))
    }

    /// The handler whose subscription a presented token authorises.
    /// Possession is the proof: a forged or retired token is simply
    /// absent from the map.
    pub fn resolve(&self, token: Bytes) -> Result<Arc<S>, Status> {
        self.sessions
            .get(&Token::from(token))
            .map(|handler| handler.clone())
            .ok_or_else(|| Status::unauthenticated("Unknown session token"))
    }

    /// A receiver observing every completed teardown, by token. A test
    /// subscribes before triggering teardown, then awaits its token:
    /// once seen, the subscription is fully gone (the token no longer
    /// resolves and its registration is unregistered), so the next call
    /// is rejected without a race.
    #[cfg(test)]
    pub fn torn_down(&self) -> broadcast::Receiver<Token> {
        self.torn_down.subscribe()
    }

    // One subscription, start to finish. Owning the whole of it keeps
    // every ending on one path: whatever committed here is unregistered
    // here.
    async fn serve(
        self,
        mut requests: Streaming<S::Request>,
        response_tx: oneshot::Sender<Result<Response<SessionStream<S::Event>>, Status>>,
    ) {
        // The wire requires Register first, so registration runs before
        // the rpc answers and a refusal is a plain error on the call.
        let register = match requests.message().await {
            Ok(Some(request)) => S::into_register(request),
            Ok(None) => None,
            Err(status) => {
                let _ = response_tx.send(Err(status));
                return;
            }
        };
        let Some(register) = register else {
            let _ = response_tx.send(Err(Status::invalid_argument(
                "The first message must be Register",
            )));
            return;
        };

        let cancel = self.tasks.child_token();
        let (events_tx, events_rx) = mpsc::channel(S::EVENT_DEPTH);
        let (handler, registration) =
            match S::register(&self.bpa, register, cancel, events_tx).await {
                Ok(registered) => registered,
                Err(status) => {
                    let _ = response_tx.send(Err(status));
                    return;
                }
            };

        // The stream leads with the Registration by construction, ahead
        // of anything the BPA fired into the channel from inside
        // `register_*`.
        let stream = handler.session().stream(registration, events_rx);

        // Published before the rpc answers: from that moment the
        // Registration can reach the client, and the token it carries
        // has to resolve already. The token is 128 unguessable bits, so
        // nobody can present it before reading it off this stream.
        let token = handler.session().token().clone();
        self.sessions.insert(token.clone(), handler.clone());

        if response_tx.send(Ok(Response::new(stream))).is_err() {
            // The rpc did not survive its own registration, so nothing
            // will ever hold the subscription: undo what registering
            // just committed.
            self.sessions.remove(&token);
            handler.unregister().await;
            return;
        }

        self.handle_requests(&handler, requests).await;
    }

    // Listens to one subscription's request stream until it ends, then
    // runs the one teardown. The endings every surface shares are
    // decided here; what a message means is the handler's. Ending and
    // unwinding are one function so no ending can leave the
    // subscription published.
    async fn handle_requests(&self, handler: &Arc<S>, mut requests: Streaming<S::Request>) {
        let cancelled = handler.session().cancellation();
        loop {
            let request = tokio::select! {
                biased;
                _ = cancelled.cancelled() => break,
                request = requests.message() => request,
            };
            match request {
                Ok(Some(request)) => {
                    if handler.on_request(request).await.is_break() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("Subscription stream failed: {e}");
                    break;
                }
            }
        }

        // Ordered by what the client must observe as done: the stream
        // closes last, so by the time the client sees teardown the token
        // is dead and the registration's identity is reusable. Work the
        // handler was holding stays queued in the BPA for a later
        // registration.
        let token = handler.session().token().clone();
        self.sessions.remove(&token);
        handler.unregister().await;
        handler.session().abort();

        // The subscription is fully retired.
        #[cfg(test)]
        let _ = self.torn_down.send(token);
    }
}

// Manual, so the derive does not demand `S: Clone`: only the handles
// are cloned, never a handler.
impl<S: SubscribeHandler> Clone for Sessions<S> {
    fn clone(&self) -> Self {
        Self {
            bpa: self.bpa.clone(),
            tasks: self.tasks.clone(),
            sessions: self.sessions.clone(),
            #[cfg(test)]
            torn_down: self.torn_down.clone(),
        }
    }
}
