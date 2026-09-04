// The rpc services, one file per wire surface, and the shared session
// machinery: [`watch_session`] parks on a session stream until it ends,
// and [`Deliveries`] holds a session's announced streams for its Receive
// door. A delivery's two ends are [`Deliveries::serve`] (the on-deliver
// side, forwarding into the rendezvous) and `adapter::Writer` (the
// Receive side, draining it onto the wire). Each surface keeps its doors
// next to the schema they implement; `application.rs` is the template,
// and the messages' `Chunk`/`Cancel`/`Unregister` capabilities live with
// the wire contract in `crate::stream`.

pub mod application;
pub mod cla;
pub mod routing;
pub mod service;

use core::{future::Future, pin::pin, time::Duration};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use hardy_async::{CancellationToken, TaskPool, channel, sync::spin::Once};
use hardy_bpa::{
    bpa::BpaRegistration,
    services::{self, Error},
    stream::{Receiver, Segment},
};
use prost_types::Timestamp;
use time::OffsetDateTime;
// The mpsc `Receiver` collides with the stream `Receiver` trait above, so the
// event channel's receiving half is aliased where it is the less central name.
use tokio::sync::mpsc::{self, Receiver as EventReceiver};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Status, Streaming};
use tracing::{debug, error, warn};

use crate::{
    error_status::embed_service_error,
    server::{
        DATA_CHANNEL_DEPTH, adapter,
        session::{Session, SessionStream, Sessions},
    },
    stream::{Ack, Cancel, Chunk, Unregister},
};

// Both types are foreign, so this is a free function rather than a
// `From`; shared by the surfaces that emit status-report timestamps.
fn to_timestamp(t: OffsetDateTime) -> Timestamp {
    Timestamp {
        seconds: t.unix_timestamp(),
        nanos: t.nanosecond() as i32,
    }
}

// The one point where BPA service errors become gRPC statuses, shared
// by every surface's doors. The typed discriminator is embedded on the
// way out so the SDK can recover the exact variant past the coarse code.
fn service_status(error: Error) -> Status {
    let status = match &error {
        Error::ServiceIdInUse(_) | Error::DuplicateBundle => {
            Status::already_exists(error.to_string())
        }
        Error::InvalidDestination(_)
        | Error::InvalidSource(_)
        | Error::InvalidBundle(_)
        | Error::PayloadUnderrun { .. } => Status::invalid_argument(error.to_string()),
        Error::NodeId(_) => Status::failed_precondition(error.to_string()),
        Error::PayloadTooLarge { .. } | Error::PayloadUnaddressable { .. } => {
            Status::resource_exhausted(error.to_string())
        }
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::StreamCancelled => Status::cancelled(error.to_string()),
        Error::Dropped(_) => Status::aborted(error.to_string()),
        // The internal chain may carry host detail an untrusted peer
        // must never see: log it server-side and ship a generic status.
        // The kind is still embedded, so the SDK learns it was internal.
        Error::Internal(e) => {
            error!("internal service error: {e}");
            Status::internal("internal error")
        }
    };
    embed_service_error(status, &error)
}

// One session's watch: parks until the session ends, however it ends
// — the trigger firing, the client's Unregister, half-close, or a
// failed stream. Unexpected messages are ignored without being
// Debug-formatted, uniformly with the data-plane doors.
async fn watch_session<Req: Unregister>(
    cancelled: CancellationToken,
    mut requests: Streaming<Req>,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => return,
            request = requests.message() => match request {
                Ok(Some(request)) if request.is_unregister() => return,
                Ok(Some(_)) => warn!("Ignoring unexpected message on the session stream"),
                Ok(None) | Err(_) => return,
            },
        }
    }
}

// One gRPC surface's component as the BPA sees it, reduced to what the
// generic session machinery needs: its session state, and how to
// unregister its sink on teardown. The concrete component keeps its
// surface-specific event doors; only the lifecycle is shared.
pub(super) trait Component: Send + Sync + 'static {
    type Event: Send + 'static;

    fn session(&self) -> &Session<Self::Event>;

    // Unregisters the component's sink from the BPA, if it ever
    // registered one. The sink type is surface-specific, so the call is
    // the component's; a repeat unregister no-ops inside the sink. The
    // future is `Send`: the bridge drives it from a spawned session task.
    fn unregister_sink(&self) -> impl Future<Output = ()> + Send;
}

// A `Once`-cell holding one surface's sink: set once at registration,
// read on every data-plane door. `T` is the surface's sink trait
// object, so the cell is written as a `Box` and shared as an `Arc`.
pub(super) struct SinkSlot<T: ?Sized>(Once<Arc<T>>);

impl<T: ?Sized> SinkSlot<T> {
    pub(super) fn new() -> Self {
        Self(Once::new())
    }

    // Records the sink handed in at registration; the BPA calls this
    // exactly once, so a second set is ignored.
    pub(super) fn set(&self, sink: Box<T>) {
        self.0.call_once(|| Arc::from(sink));
    }

    // The sink for a data-plane door, or the unregistered status a call
    // arriving before (or racing) registration must answer.
    pub(super) fn get(&self) -> Result<Arc<T>, Status> {
        self.0
            .get()
            .cloned()
            .ok_or_else(|| Status::unavailable("Unregistered"))
    }

    // The sink for the teardown path, absent if none ever registered.
    pub(super) fn peek(&self) -> Option<Arc<T>> {
        self.0.get().cloned()
    }
}

impl<T: ?Sized> Default for SinkSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// One surface's session bridge: the BPA handle, the pool the sessions
/// run on, and the live-session index. Shutting down the pool tears the
/// sessions and drives unregistration, so shut it down only after the
/// transport has stopped accepting.
pub(super) struct Bridge<C: Component> {
    pub bpa: Arc<dyn BpaRegistration>,
    pub tasks: TaskPool,
    pub sessions: Arc<Sessions<C>>,
}

impl<C: Component> Bridge<C> {
    pub(super) fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            bpa,
            tasks,
            sessions: Arc::new(Sessions::new()),
        }
    }

    // Publishes a freshly registered component's session and spawns the
    // one task it owns: the task waits out the session's life, then
    // unregisters it. Returns the response stream the handler answers
    // with. `requests` is the Subscribe request side, watched for the
    // client's Unregister or half-close; `label` names the task.
    pub(super) fn open_session<Req>(
        &self,
        component: Arc<C>,
        registration: C::Event,
        events: EventReceiver<Result<C::Event, Status>>,
        requests: Streaming<Req>,
        label: &'static str,
    ) -> SessionStream<C::Event>
    where
        Req: Unregister + Send + 'static,
        C::Event: Unpin,
    {
        // Published before the client can know its token.
        self.sessions
            .publish(component.session().token().clone(), component.clone());

        // The session ends on Unregister or half-close, or on the
        // trigger: the stream's guard (the rpc dying), `on_unregister`,
        // or pool shutdown.
        let cancelled = component.session().cancellation();
        let stream = component.session().stream(registration, events);

        let bridge = self.clone();
        let task = async move {
            watch_session(cancelled, requests).await;
            bridge.unregister_session(component).await;
        };
        #[cfg(feature = "instrument")]
        {
            let span = tracing::trace_span!(parent: None, "grpc_session", surface = label);
            span.follows_from(tracing::Span::current());
            self.tasks
                .spawn(tracing::Instrument::instrument(task, span));
        }
        #[cfg(not(feature = "instrument"))]
        {
            let _ = label;
            self.tasks.spawn(task);
        }
        stream
    }

    // Unregisters one session, however it ended. Work the component was
    // holding (parked deliveries, in-flight forwardings) stays queued in
    // the BPA for a later registration; a repeat unregister no-ops
    // inside the sink.
    async fn unregister_session(&self, component: Arc<C>) {
        // Ordered by what the client must observe as done. Retire the
        // token, then unregister from the BPA (which frees the
        // registration's identity before firing on_unregister), then
        // close the stream last: the client sees teardown via the stream
        // closing, so once it does the token is dead and the identity is
        // reusable.
        self.sessions.remove(component.session().token());
        component.unregister_sink().await;
        component.session().abort();
        // The teardown barrier for tests: the session is fully retired.
        #[cfg(test)]
        self.sessions.signal_torn_down(component.session().token());
    }
}

// Manual, so the derive does not demand `C: Clone`: only the three
// handles are cloned, never the component.
impl<C: Component> Clone for Bridge<C> {
    fn clone(&self) -> Self {
        Self {
            bpa: self.bpa.clone(),
            tasks: self.tasks.clone(),
            sessions: self.sessions.clone(),
        }
    }
}

// Dead-entry sweep threshold for [`Deliveries`]: below it the map is
// too small to matter, above it each hold first reclaims entries
// whose bundle has certainly died (expiry passed), so the map tracks
// the parked set rather than the session's announcement history.
const DELIVERIES_SWEEP_THRESHOLD: usize = 64;

// A held delivery awaiting its Receive: the bundle's expiry (for the
// dead-entry sweep), the rendezvous the door drains, and the channel the
// door reports the client's verdict back through (a `()` sent =
// committed, the sender dropped = abandoned).
struct Held {
    expiry: OffsetDateTime,
    stream: Box<dyn Receiver<Segment>>,
    commit: channel::Sender<()>,
}

#[derive(Default)]
struct DeliveriesState {
    held: HashMap<String, Held>,
    // The map's size at the last dead-entry sweep. The next sweep waits
    // until the map has doubled since, so a burst of still-live holds is
    // scanned O(1) amortised (O(n) total) instead of the whole map on
    // every insert.
    last_swept_len: usize,
}

// One session's announced-but-uncollected deliveries: the stream each
// `on_deliver` carried, with its bundle's expiry, keyed by the wire
// form of its bundle id. Dropping the map on session death leaves the
// bundles parked in the BPA.
//
// A `std::sync::Mutex`, not the spin lock cla.rs uses for its
// forwardings map: the sweep below holds the lock across an O(n) retain,
// which a spin lock must never do. The asymmetry is deliberate.
#[derive(Default)]
struct Deliveries(Mutex<DeliveriesState>);

impl Deliveries {
    // Holds an announced stream for the Receive door. A bundle can die
    // while parked (expiry is the latest that can happen) with no
    // withdraw signal to the bridge, so dead entries are reclaimed
    // here, amortised against holds.
    fn hold(&self, key: String, held: Held) {
        let mut state = self.0.lock().expect("deliveries lock poisoned");
        // Sweep only once the map is both large enough to matter and has
        // doubled since the last sweep: doubling keeps the total scan
        // work linear in the number of holds.
        if state.held.len() >= DELIVERIES_SWEEP_THRESHOLD
            && state.held.len() >= state.last_swept_len * 2
        {
            let now = OffsetDateTime::now_utc();
            state.held.retain(|_, held| held.expiry > now);
            state.last_swept_len = state.held.len();
        }
        state.held.insert(key, held);
    }

    // Withdraws a held stream: a dead session dropping its
    // announcement, so the BPA announces the parked bundle again to a
    // later registration.
    fn withdraw(&self, key: &str) {
        self.0
            .lock()
            .expect("deliveries lock poisoned")
            .held
            .remove(key);
    }

    // Takes the single collection capability for an announcement, the
    // rendezvous and the verdict channel included. A repeat claim reads
    // as not-found.
    fn claim(&self, key: &str) -> Option<Held> {
        self.0
            .lock()
            .expect("deliveries lock poisoned")
            .held
            .remove(key)
    }

    // The whole `on_deliver` side of a delivery: hold a rendezvous under
    // `key`, run `announce` to emit the surface's Delivery event, relay
    // `stream` into the rendezvous, then wait for the client's verdict.
    // Returns `Ok` once the client has received the whole bundle and
    // acknowledged it with an in-band `Ack` (the wire's commit point), and
    // `Err` when it abandoned the collection, the session died, or the
    // hold expired, parking the bundle for a later registration. The
    // rendezvous and the commit one-shot are private to this type; a
    // surface supplies only the event.
    async fn serve<A, F>(
        &self,
        key: String,
        expiry: OffsetDateTime,
        cancel: CancellationToken,
        stream: &mut dyn Receiver<Segment>,
        announce: A,
    ) -> services::Result<()>
    where
        A: FnOnce() -> F,
        F: Future<Output = bool>,
    {
        let (tx, rx) = channel::bounded(0);
        let (commit_tx, commit_rx) = channel::bounded::<()>(1);
        // Held before the event goes out so a client racing its Receive
        // against the announcement always finds the entry.
        self.hold(
            key.clone(),
            Held {
                expiry,
                stream: Box::new(rx),
                commit: commit_tx,
            },
        );
        if !announce().await {
            self.withdraw(&key);
            return Err(Error::Disconnected);
        }

        // Relay the delivery into the rendezvous the Receive door drains,
        // then wait for the client's verdict on `commit_rx`: an in-band
        // `Ack` (having received the whole bundle) commits, everything else
        // parks. The whole exchange is bounded by the bundle's expiry (the
        // BPA cannot see the wire's announce/collect split, so capping the
        // hold is this layer's job); a lost connection is the session's
        // cancel; and a rendezvous send failing means the door went away.
        let hold = (expiry - OffsetDateTime::now_utc())
            .try_into()
            .unwrap_or(Duration::ZERO);
        let delivery = tokio::time::timeout(hold, async {
            loop {
                let segment = stream.recv().await.map_err(|_| Error::StreamCancelled)?;
                let last = matches!(segment, Segment::Final(_));
                tx.send(segment).await.map_err(|_| Error::StreamCancelled)?;
                if last {
                    break;
                }
            }
            commit_rx.recv().await.map_err(|_| Error::StreamCancelled)
        });
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Disconnected),
            r = delivery => r.unwrap_or(Err(Error::StreamCancelled)),
        };
        if result.is_err() {
            self.withdraw(&key);
        }
        result
    }

    // The whole Receive-door side: claim the rendezvous for `key`, probe it
    // (a delivery that died while parked answers not-found here, before the
    // response stream opens), then spawn the drain that streams it to the
    // client as `Resp` chunks and reports the verdict back to `serve`.
    // Returns the response stream.
    async fn collect<Req, Resp>(
        &self,
        tasks: &TaskPool,
        cancel: CancellationToken,
        key: &str,
        requests: Streaming<Req>,
    ) -> Result<ReceiverStream<Result<Resp, Status>>, Status>
    where
        Req: Ack + Cancel + Send + 'static,
        Resp: Chunk + Cancel + Send + 'static,
    {
        let Held {
            mut stream, commit, ..
        } = self
            .claim(key)
            .ok_or_else(|| Status::not_found("No such delivery"))?;
        let first = stream
            .recv()
            .await
            .map_err(|_| Status::not_found("No such delivery"))?;

        let (tx, rx) = mpsc::channel(DATA_CHANNEL_DEPTH);
        let writer = adapter::Writer::new(tx.clone(), cancel.clone());
        hardy_async::spawn!(tasks, "delivery_receive", async move {
            // Push the whole delivery, then let the client's in-band `Ack`
            // commit it: the commit point is the client acknowledging
            // receipt, not the in-process handoff. The whole collection
            // reduces to one `Result`: `Ok` commits (the drain reached the
            // last chunk and the client then acked); `Err` parks the
            // bundle, carrying the status ending the call, absent when the
            // writer already ended the response itself. A verdict
            // overtaking the drain can never commit, so its arm drops the
            // drain (which is the abandonment `serve` observes): an
            // abandonment is honoured at once, and an early ack is a
            // protocol violation, since a conforming client cannot ack
            // data it has not received and honouring one would finalize a
            // bundle the client provably does not hold.
            let mut verdict = pin!(verdict(requests, cancel));
            let outcome = tokio::select! {
                biased;
                early = &mut verdict => Err(Some(match early {
                    // An ack racing the drain is the protocol violation;
                    // an abandonment keeps its own status.
                    Ok(()) => Status::invalid_argument("Ack before the final chunk"),
                    Err(status) => status,
                })),
                pushed = writer.write_all(first, stream) => if pushed.is_continue() {
                    // The final chunk is on the wire; the verdict decides.
                    verdict.await.map_err(Some)
                } else {
                    Err(None)
                },
            };
            match outcome {
                // Parking is the `Err` paths dropping `commit` without
                // this send, which `serve` reads as abandoned.
                Ok(()) => {
                    let _ = commit.send(()).await;
                }
                Err(Some(status)) => {
                    let _ = tx.try_send(Err(status));
                }
                // The writer already terminated the response (session
                // cancel, a withdrawn stream, or a gone client).
                Err(None) => {}
            }
            // The response stayed open until the verdict so the client's
            // `Ack` (sent after the last chunk) was observable; closing it
            // earlier would finalize the RPC. Let it close now.
            drop(tx);
        });
        Ok(ReceiverStream::new(rx))
    }
}

// The client's verdict on a collection, read from its request side: an
// in-band `Ack` (the SDK's signal that it received the whole delivery)
// commits it; everything else abandons it, parking the bundle, with the
// status the call should end with. Silence must never commit (a crashed
// client is silent), so a half-close without an ack is an abandonment
// too, not the inert ending it is on the session stream. Unexpected
// messages are ignored.
async fn verdict<Req: Ack + Cancel>(
    mut requests: Streaming<Req>,
    cancelled: CancellationToken,
) -> Result<(), Status> {
    loop {
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => return Err(Status::aborted("Session closed")),
            message = requests.message() => match message {
                Ok(Some(req)) if req.is_ack() => return Ok(()),
                Ok(Some(req)) if req.is_cancel() => {
                    return Err(Status::cancelled("Collection abandoned"));
                }
                // Not Debug-formatted: a stray metadata message carries the
                // session token, which must never reach the logs.
                Ok(Some(_)) => warn!("Ignoring unexpected message on the Receive request side"),
                Ok(None) => return Err(Status::cancelled("Collection abandoned")),
                Err(e) => {
                    debug!("Receive stream failed: {e}");
                    return Err(Status::aborted("Receive stream failed"));
                }
            },
        }
    }
}

// The shared halves of every surface's inline wire test: the generic
// harness plumbing (a running BPA, the port-0 serve dance, the regression
// timeout, the teardown barrier), cross-imported by the four surface test
// modules. Surface-specific fixtures (the typed client, `register`,
// `send`, `collect`) stay local to each surface.
#[cfg(test)]
pub mod tests {
    use std::{net::SocketAddr, time::Duration};

    use hardy_bpa::{Bytes, bpa::Bpa, node_ids::NodeIds};
    use hardy_bpv7::eid::{IpnNodeId, NodeId};
    // `broadcast::Receiver` collides with the stream `Receiver` trait that
    // `super::*` brings in, so the teardown receiver is aliased.
    use tokio::{net::TcpListener, sync::broadcast::Receiver as TornReceiver};
    use tonic::transport::server::{Router, TcpIncoming};

    use super::*;
    use crate::server::token::SessionToken;

    // A generous hang failsafe on an event-driven wait; the timeout only
    // bounds a regression.
    pub async fn timeout<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("test timed out")
    }

    // The single-node ipn:1 configuration every surface's default harness
    // uses.
    pub fn ipn1() -> NodeIds {
        NodeIds::try_from(
            [NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            })]
            .as_slice(),
        )
        .unwrap()
    }

    pub fn build_bundle(source: &str, destination: &str, payload: &[u8]) -> Bytes {
        let (_, data) = hardy_bpv7::builder::Builder::new(
            source.parse().unwrap(),
            destination.parse().unwrap(),
        )
        .with_payload(std::borrow::Cow::Borrowed(payload))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        Bytes::from(data)
    }

    // A started BPA behind the bridge; `status_reports` is on only for the
    // surfaces whose report round-trip tests need it.
    pub async fn build_bpa(node_ids: NodeIds, status_reports: bool) -> Arc<Bpa> {
        let bpa = Arc::new(
            Bpa::builder()
                .node_ids(node_ids)
                .status_reports(status_reports)
                .build()
                .await
                .unwrap(),
        );
        bpa.start(false);
        bpa
    }

    // Serves `router` on a fresh port-0 listener and returns its address:
    // the shared listener + serve_with_incoming dance, parameterised by
    // the surface's already-built tonic service router.
    pub async fn serve(router: Router) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));
        tokio::spawn(router.serve_with_incoming(incoming));
        address
    }

    // Awaits the teardown barrier for `token`: once its teardown signal
    // fires, the session is fully retired (the token no longer resolves
    // and its registration is unregistered), so a later call is rejected
    // without a race. Subscribe before triggering teardown. The timeout
    // only bounds a regression.
    pub async fn wait_torn_down(torn: &mut TornReceiver<SessionToken>, token: &Bytes) {
        timeout(async { while Bytes::from(torn.recv().await.unwrap()) != *token {} }).await;
    }

    fn held(seconds_from_now: i64) -> Held {
        let (commit, _verdict) = channel::bounded::<()>(1);
        Held {
            expiry: OffsetDateTime::now_utc() + time::Duration::seconds(seconds_from_now),
            stream: Box::new(Bytes::from_static(b"x")),
            commit,
        }
    }

    #[test]
    fn a_claim_is_single_use_and_withdraw_removes() {
        let deliveries = Deliveries::default();
        deliveries.hold("a".to_string(), held(60));

        assert!(deliveries.claim("a").is_some());
        assert!(deliveries.claim("a").is_none(), "a claim is single-use");

        deliveries.hold("b".to_string(), held(60));
        deliveries.withdraw("b");
        assert!(deliveries.claim("b").is_none());
    }

    #[test]
    fn dead_entries_are_swept_once_the_threshold_is_reached() {
        let deliveries = Deliveries::default();
        for i in 0..DELIVERIES_SWEEP_THRESHOLD {
            deliveries.hold(format!("dead-{i}"), held(-60));
        }
        // The hold at the threshold reclaims every expired entry first.
        deliveries.hold("live".to_string(), held(60));

        assert_eq!(
            deliveries
                .0
                .lock()
                .expect("deliveries lock poisoned")
                .held
                .len(),
            1,
            "expired entries must be reclaimed, not accumulated"
        );
        assert!(deliveries.claim("live").is_some());
        assert!(deliveries.claim("dead-0").is_none());
    }
}
