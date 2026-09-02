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

use core::{
    future::{Future, pending},
    time::Duration,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use hardy_async::{CancellationToken, TaskPool, channel, sync::spin::Once};
use hardy_bpa::{
    Bytes,
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
    stream::{Cancel, Chunk, Unregister},
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

// Forwards one delivery segment into the rendezvous the Receive door
// drains, per the commit-marker protocol: a `Next` travels as-is, and the
// terminal `Final` splits into its data (a `Next`) then an empty `Final`
// marker. The door's claim-time probe pulls the data without committing,
// so a client that takes the bytes and then abandons (the receiver
// dropped before the marker) fails the marker send, deferring the bundle
// to the next registration; an already-empty terminal segment is just the
// marker. Returns whether that marker was sent (the delivery is
// complete); a closed rendezvous is [`Error::StreamCancelled`].
async fn forward_segment(
    tx: &channel::Sender<Segment>,
    segment: Segment,
) -> services::Result<bool> {
    match segment {
        Segment::Next(b) => {
            tx.send(Segment::Next(b))
                .await
                .map_err(|_| Error::StreamCancelled)?;
            Ok(false)
        }
        Segment::Final(b) => {
            if !b.is_empty() {
                tx.send(Segment::Next(b))
                    .await
                    .map_err(|_| Error::StreamCancelled)?;
            }
            tx.send(Segment::Final(Bytes::new()))
                .await
                .map_err(|_| Error::StreamCancelled)?;
            Ok(true)
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

type HeldStream = (OffsetDateTime, Box<dyn Receiver<Segment>>);

#[derive(Default)]
struct DeliveriesState {
    held: HashMap<String, HeldStream>,
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
    fn hold(&self, key: String, expiry: OffsetDateTime, stream: Box<dyn Receiver<Segment>>) {
        let mut state = self.0.lock().expect("deliveries lock poisoned");
        // Sweep only once the map is both large enough to matter and has
        // doubled since the last sweep: doubling keeps the total scan
        // work linear in the number of holds.
        if state.held.len() >= DELIVERIES_SWEEP_THRESHOLD
            && state.held.len() >= state.last_swept_len * 2
        {
            let now = OffsetDateTime::now_utc();
            state.held.retain(|_, (expiry, _)| *expiry > now);
            state.last_swept_len = state.held.len();
        }
        state.held.insert(key, (expiry, stream));
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

    // Takes the single collection capability for an announcement; a
    // repeat claim reads as not-found.
    fn claim(&self, key: &str) -> Option<Box<dyn Receiver<Segment>>> {
        self.0
            .lock()
            .expect("deliveries lock poisoned")
            .held
            .remove(key)
            .map(|(_, stream)| stream)
    }

    // The whole `on_deliver` side of a delivery: hold a rendezvous under
    // `key`, run `announce` to emit the surface's Delivery event, then
    // relay `stream` to the client. Returns `Ok` once the client has taken
    // the whole bundle (the wire's commit point) and `Err` when it
    // abandoned the collection, the session died, or the hold expired,
    // parking the bundle for a later registration. The rendezvous and its
    // commit-marker protocol are private to this type; a surface supplies
    // only the event.
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
        // Held before the event goes out so a client racing its Receive
        // against the announcement always finds the entry.
        self.hold(key.clone(), expiry, Box::new(rx));
        if !announce().await {
            self.withdraw(&key);
            return Err(Error::Disconnected);
        }

        // Relay the delivery into the rendezvous, bounded by the bundle's
        // expiry: the BPA cannot see the wire's announce/collect split, so
        // capping the hold is this layer's job. The rendezvous is
        // `bounded(0)`, so forwarding the terminal marker completes only
        // when the client takes it (the wire's commit point). Any failure
        // parks the bundle for a later registration: the session's cancel
        // (a lost connection), a far end that went away (client abandoned,
        // or the hold expired) failing a send, or a stalled source.
        let hold = (expiry - OffsetDateTime::now_utc())
            .try_into()
            .unwrap_or(Duration::ZERO);
        let relay = tokio::time::timeout(hold, async {
            loop {
                match stream.recv().await {
                    // Forwarding the terminal segment's marker completes
                    // the relay; every other segment continues it.
                    Ok(segment) => {
                        if forward_segment(&tx, segment).await? {
                            break Ok(());
                        }
                    }
                    Err(_) => break Err(Error::StreamCancelled),
                }
            }
        });
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Disconnected),
            r = relay => r.unwrap_or(Err(Error::StreamCancelled)),
        };
        if result.is_err() {
            self.withdraw(&key);
        }
        result
    }

    // The whole Receive-door side: claim the rendezvous for `key`, probe it
    // (a delivery that died while parked answers not-found here, before the
    // response stream opens), then spawn the drain that streams it to the
    // client as `Resp` chunks while watching `requests` for an in-band
    // abandonment. Returns the response stream.
    async fn collect<Req, Resp>(
        &self,
        tasks: &TaskPool,
        cancel: CancellationToken,
        key: &str,
        requests: Streaming<Req>,
    ) -> Result<ReceiverStream<Result<Resp, Status>>, Status>
    where
        Req: Cancel + Send + 'static,
        Resp: Chunk + Cancel + Send + 'static,
    {
        let mut stream = self
            .claim(key)
            .ok_or_else(|| Status::not_found("No such delivery"))?;
        let first = stream
            .recv()
            .await
            .map_err(|_| Status::not_found("No such delivery"))?;

        let (tx, rx) = mpsc::channel(DATA_CHANNEL_DEPTH);
        let writer = adapter::Writer::new(tx.clone(), cancel);
        hardy_async::spawn!(tasks, "delivery_receive", async move {
            // The client can abandon the collection in-band while it
            // drains. Its request side reduces to a terminal status: a
            // cancel is the abandonment, a failed stream is treated the
            // same (a partial collection must never look complete), and a
            // half-close or unexpected message leaves it inert (a client
            // may send only the metadata). Checked first, it preempts the
            // drain, ends the call, and parks the bundle for a later
            // registration; it races the whole write, so a cancel landing
            // during the final flush (once the delivery has committed in
            // the BPA) still ends the call here.
            tokio::select! {
                biased;
                status = async move {
                    let mut requests = requests;
                    loop {
                        match requests.message().await {
                            Ok(Some(req)) if req.is_cancel() => {
                                break Status::cancelled("Collection abandoned");
                            }
                            // Not Debug-formatted: a stray metadata message
                            // carries the session token, which must never
                            // reach the logs.
                            Ok(Some(_)) => {
                                warn!("Ignoring unexpected message on the Receive request side")
                            }
                            Ok(None) => pending().await,
                            Err(e) => {
                                debug!("Receive stream failed: {e}");
                                break Status::aborted("Receive stream failed");
                            }
                        }
                    }
                } => {
                    let _ = tx.try_send(Err(status));
                }
                _ = writer.write_all(first, stream) => {}
            }
        });
        Ok(ReceiverStream::new(rx))
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

    fn held(seconds_from_now: i64) -> (OffsetDateTime, Box<dyn Receiver<Segment>>) {
        (
            OffsetDateTime::now_utc() + time::Duration::seconds(seconds_from_now),
            Box::new(Bytes::from_static(b"x")),
        )
    }

    #[test]
    fn a_claim_is_single_use_and_withdraw_removes() {
        let deliveries = Deliveries::default();
        let (expiry, stream) = held(60);
        deliveries.hold("a".to_string(), expiry, stream);

        assert!(deliveries.claim("a").is_some());
        assert!(deliveries.claim("a").is_none(), "a claim is single-use");

        let (expiry, stream) = held(60);
        deliveries.hold("b".to_string(), expiry, stream);
        deliveries.withdraw("b");
        assert!(deliveries.claim("b").is_none());
    }

    #[test]
    fn dead_entries_are_swept_once_the_threshold_is_reached() {
        let deliveries = Deliveries::default();
        for i in 0..DELIVERIES_SWEEP_THRESHOLD {
            let (expiry, stream) = held(-60);
            deliveries.hold(format!("dead-{i}"), expiry, stream);
        }
        // The hold at the threshold reclaims every expired entry first.
        let (expiry, stream) = held(60);
        deliveries.hold("live".to_string(), expiry, stream);

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
