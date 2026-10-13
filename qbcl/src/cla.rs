//! The convergence layer adapter: [`Qbcl`] registers with a BPA, carries the bundles the BPA forwards over the reliable service of Section 4.3, and dispatches the bundles peers send to the BPA.
//!
//! Sessions are dialed on demand and kept in a registry keyed by the peer's address, which a forward to a [`ClaAddress::Quic`] looks up. Each session is reported to the BPA as a peer with the Node IDs its certificate carries, and removed when it closes, including by idle timeout (Section 4.5).

use core::num::NonZeroU64;
use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

use bytes::Bytes;
use hardy_async::{
    TaskPool,
    sync::{Mutex, spin::Once},
};
use hardy_bpa::{
    async_trait,
    cla::{
        self, Acceptance, Cla, ClaAddress, ClaAddressType, ClaInit, ForwardBundleResult, Segment,
        Sink, TransferOutcome,
    },
    stream::{Receiver, RecvError},
};
use hardy_bpv7::{bundle::Id, eid::NodeId};
use thiserror::Error;
// Aliased: the dial lock is held across an await, which a blocking mutex
// must not be.
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info, warn};

use crate::{
    endpoint::{self, Accepting, Endpoint, Session, Unreliable},
    error_code::ErrorCode,
    reliable::{self, BundleReceiver},
    stream::{BundleVersion, Form, Pushed, StreamHead},
    tls::Tls,
};

/// A specialized `Result` type for building a [`Qbcl`].
pub type Result<T> = core::result::Result<T, Error>;

/// Errors from [`QbclBuilder::build`].
#[derive(Debug, Error)]
pub enum Error {
    /// No address to listen on was given.
    #[error("no listen address was given")]
    NoListenAddress,

    /// No TLS material was given.
    #[error("no TLS material was given")]
    NoTls,

    /// The QUIC endpoint could not bind its UDP socket.
    #[error("failed to bind the QUIC endpoint: {0}")]
    Bind(#[source] io::Error),
}

/// Builder for [`Qbcl`]. Obtain one from [`Qbcl::builder`], chain the inputs, then bind the endpoint with [`build()`](Self::build).
#[must_use = "a QbclBuilder does nothing unless `build()` is called"]
#[derive(Default)]
pub struct QbclBuilder {
    listen: Option<SocketAddr>,
    tls: Option<Tls>,
    max_bundle_size: Option<NonZeroU64>,
    peers: HashMap<SocketAddr, NodeId>,
}

impl QbclBuilder {
    /// The UDP address the endpoint binds, mandatory. Sessions are dialed from it as well as accepted on it, so a peer's sessions come from the address it listens on. Port 0 binds an ephemeral port, for a node that only dials.
    pub fn listen(mut self, addr: SocketAddr) -> Self {
        self.listen = Some(addr);
        self
    }

    /// The node's TLS material, mandatory.
    pub fn tls(mut self, tls: Tls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// The largest bundle the node accepts from a peer, declared to the BPA at registration. A transfer over the limit, after the BPA folds in its own, is cancelled with `QBCL_BUNDLE_TOO_LARGE`. Unlimited by default.
    pub fn max_bundle_size(mut self, max: NonZeroU64) -> Self {
        self.max_bundle_size = Some(max);
        self
    }

    /// The Node ID configured for the peer at `addr`. A session dialed to `addr` is refused unless the peer's certificate carries it, or carries no Node ID at all, in which case the configured one stands in (Section 4.2).
    pub fn peer(mut self, addr: SocketAddr, node_id: NodeId) -> Self {
        self.peers.insert(addr, node_id);
        self
    }

    /// Binds the endpoint.
    ///
    /// # Errors
    ///
    /// [`Error::NoListenAddress`] or [`Error::NoTls`] when an input was not chained, or [`Error::Bind`] when the socket cannot be bound.
    pub fn build(self) -> Result<Qbcl> {
        let listen = self.listen.ok_or(Error::NoListenAddress)?;
        let tls = self.tls.ok_or(Error::NoTls)?;
        // The unreliable service is not offered until the BPA can choose
        // between the two services per bundle.
        let endpoint =
            Endpoint::new(listen, Arc::new(tls), Unreliable::NotOffered).map_err(Error::Bind)?;
        Ok(Qbcl {
            endpoint: Arc::new(endpoint),
            max_bundle_size: self.max_bundle_size,
            peers: self.peers,
            sessions: Arc::new(Sessions::default()),
            context: Once::new(),
            tasks: TaskPool::new(),
        })
    }
}

/// The Qubicle convergence layer adapter, registered with a BPA as the handler of [`ClaAddressType::Quic`] addresses.
///
/// A forward answers [`ForwardBundleResult::Accepted`] once every octet of the bundle is in QUIC's hands, and reports [`TransferOutcome::Completed`] when the peer's QUIC endpoint has acknowledged them all and the FIN (Section 4.3.1). That is receipt by the peer's transport, not acceptance by its BPA.
pub struct Qbcl {
    endpoint: Arc<Endpoint>,
    max_bundle_size: Option<NonZeroU64>,
    peers: HashMap<SocketAddr, NodeId>,
    sessions: Arc<Sessions>,
    context: Once<Arc<Context>>,
    tasks: TaskPool,
}

// What the session and transfer tasks share, fixed at registration.
struct Context {
    sink: Arc<dyn Sink>,
    // The effective limit the BPA reported at registration.
    max_bundle_size: Option<NonZeroU64>,
    sessions: Arc<Sessions>,
    tasks: TaskPool,
}

// The open sessions, by the peer's address, and a lock per address that
// makes concurrent forwards to an unconnected peer share one dial.
#[derive(Default)]
struct Sessions {
    open: Mutex<HashMap<SocketAddr, Arc<Session>>>,
    dialing: Mutex<HashMap<SocketAddr, Arc<AsyncMutex<()>>>>,
}

impl Sessions {
    fn get(&self, addr: SocketAddr) -> Option<Arc<Session>> {
        self.open
            .lock()
            .get(&addr)
            .filter(|session| !session.is_closed())
            .cloned()
    }

    // A newer session with the same peer replaces an older one, which
    // carries on until it closes but is no longer chosen for forwards.
    fn insert(&self, session: Arc<Session>) {
        self.open.lock().insert(session.remote_address(), session);
    }

    // Removes `session` if it is still the one registered for its peer,
    // returning whether it was.
    fn remove(&self, session: &Arc<Session>) -> bool {
        let mut open = self.open.lock();
        let addr = session.remote_address();
        if open
            .get(&addr)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            open.remove(&addr);
            true
        } else {
            false
        }
    }

    fn dial_lock(&self, addr: SocketAddr) -> Arc<AsyncMutex<()>> {
        self.dialing.lock().entry(addr).or_default().clone()
    }
}

impl Qbcl {
    /// Starts building a [`Qbcl`].
    pub fn builder() -> QbclBuilder {
        QbclBuilder::default()
    }

    /// The address the endpoint is bound to.
    ///
    /// # Errors
    ///
    /// The operating system's error, if the socket's address cannot be read.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// The registration declarations to pass to the BPA with this adapter: the QUIC address type, no lane limit, since every transfer has its own stream, and the configured maximum bundle size.
    pub fn cla_init(&self) -> ClaInit {
        ClaInit {
            address_type: Some(ClaAddressType::Quic),
            lane_count: None,
            max_bundle_size: self.max_bundle_size,
        }
    }

    /// Opens a session with the peer at `addr`, unless one is already open. The peer is reported to the BPA once the session is up.
    ///
    /// # Errors
    ///
    /// [`cla::Error::Disconnected`] before registration, or [`cla::Error::Internal`] when the dial fails.
    pub async fn connect(&self, addr: SocketAddr) -> cla::Result<()> {
        let context = self.context.get().ok_or(cla::Error::Disconnected)?;
        self.session(context, addr)
            .await
            .map(|_| ())
            .map_err(|e| cla::Error::Internal(e.into()))
    }

    // The open session with `addr`, dialing one if there is none.
    async fn session(
        &self,
        context: &Arc<Context>,
        addr: SocketAddr,
    ) -> endpoint::Result<Arc<Session>> {
        if let Some(session) = self.sessions.get(addr) {
            return Ok(session);
        }
        let lock = self.sessions.dial_lock(addr);
        let _dialing = lock.lock().await;
        // The previous holder of the lock may have opened one.
        if let Some(session) = self.sessions.get(addr) {
            return Ok(session);
        }
        let session = Arc::new(
            self.endpoint
                .connect(addr, self.peers.get(&addr).cloned())
                .await?,
        );
        serve(context, session.clone());
        Ok(session)
    }
}

#[async_trait]
impl Cla for Qbcl {
    async fn on_register(
        &self,
        sink: Box<dyn Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        let mut first_registration = false;
        let context = self.context.call_once(|| {
            first_registration = true;
            Arc::new(Context {
                sink: sink.into(),
                max_bundle_size,
                sessions: self.sessions.clone(),
                tasks: self.tasks.clone(),
            })
        });
        if !first_registration {
            warn!("Registration refused: the adapter is already registered");
            return;
        }
        if max_bundle_size != self.max_bundle_size {
            info!(
                "Maximum bundle size is {max_bundle_size:?} after the BPA's own limit, configured {:?}",
                self.max_bundle_size
            );
        }

        let endpoint = self.endpoint.clone();
        let context = context.clone();
        self.tasks.spawn(async move {
            while let Some(accepting) = endpoint.accept().await {
                let context = context.clone();
                context.tasks.clone().spawn(handshake(context, accepting));
            }
        });
    }

    async fn on_unregister(&self) {
        // Every session closes at once, failing the transfers on them, and
        // every task ends with its session.
        self.endpoint.close(ErrorCode::ShuttingDown);
        self.tasks.shutdown().await;
    }

    // Writes the bundle into its stream as the BPA hands over each segment,
    // QUIC flow control holding the BPA back, then waits for the outcome in
    // a task of its own.
    async fn forward(
        &self,
        _lane: Option<u32>,
        cla_addr: &ClaAddress,
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        let context = self.context.get().ok_or(cla::Error::Disconnected)?;
        let ClaAddress::Quic(addr) = cla_addr else {
            return Ok(ForwardBundleResult::NoNeighbour);
        };

        let session = match self.session(context, *addr).await {
            Ok(session) => session,
            Err(e) => {
                debug!("Failed to open a session with {addr}: {e}");
                return Ok(ForwardBundleResult::NoNeighbour);
            }
        };
        let mut sender = match session.open_bundle(total_len).await {
            Ok(sender) => sender,
            Err(e) => {
                debug!("Failed to open a stream to {addr}: {e}");
                return Ok(ForwardBundleResult::NoNeighbour);
            }
        };

        let mut written = 0u64;
        loop {
            let (part, last) = match stream.recv().await {
                Ok(Segment::Next(part)) => (part, false),
                Ok(Segment::Final(part)) => (part, true),
                Err(RecvError) => {
                    sender.cancel(ErrorCode::TransferCancelled);
                    return Err(cla::Error::StreamCancelled);
                }
            };
            let len = u64::try_from(part.len()).expect("a buffer length fits in u64");
            written = written.saturating_add(len);
            if written > total_len {
                sender.cancel(ErrorCode::TransferCancelled);
                return Err(cla::Error::PayloadTooLarge {
                    size: written,
                    max: total_len,
                });
            }
            if !part.is_empty() {
                match sender.write(part).await {
                    Ok(()) => {}
                    // The session is gone, and with it every transfer to
                    // the peer.
                    Err(reliable::Error::ConnectionLost(e)) => {
                        debug!("The session with {addr} ended mid-transfer: {e}");
                        return Ok(ForwardBundleResult::NoNeighbour);
                    }
                    Err(e) => return Err(cla::Error::Internal(e.into())),
                }
            }
            if last {
                break;
            }
        }
        if written < total_len {
            sender.cancel(ErrorCode::TransferCancelled);
            return Err(cla::Error::PayloadUnderrun {
                size: written,
                expected: total_len,
            });
        }

        let context = context.clone();
        let bundle_id = bundle_id.clone();
        let addr = *addr;
        self.tasks.spawn(async move {
            let outcome = match sender.finish().await {
                Ok(()) => TransferOutcome::Completed,
                Err(e) => {
                    debug!("Transfer of {bundle_id} to {addr} failed: {e}");
                    TransferOutcome::Failed
                }
            };
            if let Err(e) = context.sink.transfer_outcome(&bundle_id, outcome).await {
                debug!("Failed to report the outcome of {bundle_id}: {e}");
            }
        });
        Ok(ForwardBundleResult::Accepted)
    }
}

async fn handshake(context: Arc<Context>, accepting: Accepting) {
    match accepting.session().await {
        Ok(session) => serve(&context, Arc::new(session)),
        Err(e) => debug!("Refused a session: {e}"),
    }
}

// Registers `session` and serves the bundles its peer sends until it
// closes.
fn serve(context: &Arc<Context>, session: Arc<Session>) {
    context.sessions.insert(session.clone());
    let context = context.clone();
    context.tasks.clone().spawn(async move {
        let cla_addr = ClaAddress::Quic(session.remote_address());
        if let Err(e) = context
            .sink
            .add_peer(cla_addr.clone(), session.peer_node_ids())
            .await
        {
            debug!("Failed to add the peer at {cla_addr}: {e}");
        }
        loop {
            match session.accept_bundle().await {
                Ok(receiver) => {
                    context
                        .tasks
                        .spawn(receive(context.clone(), session.clone(), receiver));
                }
                Err(e) => {
                    debug!("The session with {cla_addr} has ended: {e}");
                    break;
                }
            }
        }
        if context.sessions.remove(&session)
            && let Err(e) = context.sink.remove_peer(&cla_addr).await
        {
            debug!("Failed to remove the peer at {cla_addr}: {e}");
        }
    });
}

// Dispatches one bundle to the BPA as it arrives, and acts on the BPA's
// verdict as far as Section 4.6 allows.
async fn receive(context: Arc<Context>, session: Arc<Session>, receiver: BundleReceiver) {
    let cla_addr = ClaAddress::Quic(session.remote_address());
    let mut incoming = Incoming {
        receiver,
        max_bundle_size: context.max_bundle_size,
        received: 0,
        ended: false,
    };
    match context
        .sink
        .dispatch(
            session.peer_node_ids().first(),
            Some(&cla_addr),
            &mut incoming,
        )
        .await
    {
        // A BPA may accept a bundle before it has all of it, a duplicate
        // say. The rest is read and discarded so that the sender sees the
        // transfer complete: Section 4.6 has no code to stop a transfer
        // because the bundle was accepted.
        Ok(Acceptance::Accepted) => incoming.drain().await,
        // Once the FIN has arrived the transfer has completed at the
        // sender, and nothing can tell it otherwise.
        Ok(Acceptance::Refused) if incoming.ended => {
            warn!("The BPA refused a bundle from {cla_addr} that the sender has seen delivered");
        }
        Ok(Acceptance::Refused) => incoming.receiver.stop(ErrorCode::TransferCancelled),
        // The receiver cancels the transfer as it drops.
        Err(e) => debug!("Failed to dispatch a bundle from {cla_addr}: {e}"),
    }
}

// One arriving bundle as the BPA pulls it, with this node's size and
// version policy applied as the octets arrive. A transfer the policy
// cancels, or that fails, ends the pull with `RecvError`, so that the BPA
// discards what it has.
struct Incoming {
    receiver: BundleReceiver,
    max_bundle_size: Option<NonZeroU64>,
    received: u64,
    // The FIN has arrived and the framing checked out.
    ended: bool,
}

impl Incoming {
    // The code to cancel a transfer with, if this node will not take it.
    fn refusal(&self, head: Option<StreamHead>) -> Option<ErrorCode> {
        let over = |len: u64| self.max_bundle_size.is_some_and(|max| len > max.get());
        match head {
            Some(StreamHead {
                version: BundleVersion::V6,
                ..
            }) => Some(ErrorCode::UnsupportedBundleVersion),
            // The wrapped form declares the length up front, so an
            // oversized bundle is cancelled before its bulk arrives.
            Some(StreamHead {
                form: Form::Wrapped { len },
                ..
            }) if over(len) => Some(ErrorCode::BundleTooLarge),
            _ if over(self.received) => Some(ErrorCode::BundleTooLarge),
            _ => None,
        }
    }

    async fn drain(&mut self) {
        while let Ok(Some(_)) = self.receiver.next().await {}
    }
}

#[async_trait]
impl Receiver<Segment> for Incoming {
    async fn recv(&mut self) -> core::result::Result<Segment, RecvError> {
        if self.ended {
            return Err(RecvError);
        }
        match self.receiver.next().await {
            Ok(Some(Pushed { head, data })) => {
                let len = u64::try_from(data.len()).expect("a buffer length fits in u64");
                self.received = self.received.saturating_add(len);
                if let Some(code) = self.refusal(head) {
                    debug!("Cancelling a transfer with {code}");
                    self.receiver.stop(code);
                    return Err(RecvError);
                }
                Ok(Segment::Next(data))
            }
            Ok(None) => {
                self.ended = true;
                Ok(Segment::Final(Bytes::new()))
            }
            Err(e) => {
                debug!("A transfer failed: {e}");
                Err(RecvError)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpa::stream::concat_stream;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    use super::*;
    use crate::{
        endpoint::tests::{Ca, node_id},
        node_id::tests::bundle_eids,
    };

    // How the mock BPA answers a dispatch.
    #[derive(Clone, Copy)]
    enum Verdict {
        // Pulls the whole bundle, accepting it if it arrives whole.
        Whole,
        // Pulls the first segment, then answers.
        AfterFirst(Acceptance),
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        AddPeer(ClaAddress, Vec<NodeId>),
        RemovePeer(ClaAddress),
        // The bundle, if it arrived whole, and the verdict.
        Dispatched {
            peer_node: Option<NodeId>,
            peer_addr: Option<ClaAddress>,
            bundle: Option<Bytes>,
            acceptance: Acceptance,
        },
        Outcome(Id, TransferOutcome),
    }

    struct MockSink {
        verdict: Verdict,
        events: UnboundedSender<Event>,
    }

    #[async_trait]
    impl Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            peer_node: Option<&NodeId>,
            peer_addr: Option<&ClaAddress>,
            stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<Acceptance> {
            let (bundle, acceptance) = match self.verdict {
                Verdict::Whole => match concat_stream(stream, usize::MAX).await {
                    Ok(bundle) => (Some(bundle), Acceptance::Accepted),
                    Err(_) => (None, Acceptance::Refused),
                },
                Verdict::AfterFirst(acceptance) => {
                    stream.recv().await.unwrap();
                    (None, acceptance)
                }
            };
            self.events
                .send(Event::Dispatched {
                    peer_node: peer_node.cloned(),
                    peer_addr: peer_addr.cloned(),
                    bundle,
                    acceptance,
                })
                .unwrap();
            Ok(acceptance)
        }

        async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> cla::Result<bool> {
            self.events
                .send(Event::AddPeer(cla_addr, node_ids.to_vec()))
                .unwrap();
            Ok(true)
        }

        async fn remove_peer(&self, cla_addr: &ClaAddress) -> cla::Result<bool> {
            self.events
                .send(Event::RemovePeer(cla_addr.clone()))
                .unwrap();
            Ok(true)
        }

        async fn transfer_outcome(
            &self,
            bundle_id: &Id,
            outcome: TransferOutcome,
        ) -> cla::Result<()> {
            self.events
                .send(Event::Outcome(bundle_id.clone(), outcome))
                .unwrap();
            Ok(())
        }
    }

    // A registered adapter on an ephemeral loopback port, and what its mock
    // BPA sees.
    struct Node {
        cla: Qbcl,
        events: UnboundedReceiver<Event>,
    }

    impl Node {
        async fn new(ca: &Ca, eid: &str, verdict: Verdict, builder: QbclBuilder) -> Self {
            let cla = builder
                .listen("127.0.0.1:0".parse().unwrap())
                .tls(ca.tls(ca.identity(eid, Some(bundle_eids(&[eid])))))
                .build()
                .unwrap();
            let (events, receiver) = unbounded_channel();
            let max_bundle_size = cla.cla_init().max_bundle_size;
            cla.on_register(
                Box::new(MockSink { verdict, events }),
                &[node_id(eid)],
                max_bundle_size,
            )
            .await;
            Self {
                cla,
                events: receiver,
            }
        }

        fn addr(&self) -> ClaAddress {
            ClaAddress::Quic(self.cla.local_addr().unwrap())
        }

        async fn forward(
            &self,
            to: &Node,
            id: &Id,
            bundle: Bytes,
        ) -> cla::Result<ForwardBundleResult> {
            let len = u64::try_from(bundle.len()).unwrap();
            self.cla
                .forward(None, &to.addr(), id, len, &mut bundle.clone())
                .await
        }

        // The next event other than a peer being added.
        async fn next(&mut self) -> Event {
            loop {
                match self.events.recv().await.expect("the sink is registered") {
                    Event::AddPeer(..) => {}
                    event => return event,
                }
            }
        }
    }

    const SENDER: &str = "ipn:977000.1.0";
    const RECEIVER: &str = "ipn:977000.2.0";

    async fn pair(verdict: Verdict, receiver: QbclBuilder) -> (Node, Node) {
        let ca = Ca::new("trusted");
        let sender = Node::new(&ca, SENDER, Verdict::Whole, Qbcl::builder()).await;
        let receiver = Node::new(&ca, RECEIVER, verdict, receiver).await;
        (sender, receiver)
    }

    fn id(sequence_number: u64) -> Id {
        Id {
            source: SENDER.parse().unwrap(),
            timestamp: CreationTimestamp::from_parts(None, sequence_number),
            fragment_info: None,
        }
    }

    // A BPv7-looking bundle of `len` octets: the framing checks only the
    // first octet.
    fn bundle(len: usize) -> Bytes {
        (0..len)
            .map(|i| if i == 0 { 0x9f } else { i as u8 })
            .collect()
    }

    // Larger than quinn's default stream receive window, so that the
    // sender cannot have finished writing when the receiver answers.
    const LARGER_THAN_THE_STREAM_WINDOW: usize = 4 * 1024 * 1024;

    #[tokio::test]
    async fn a_forwarded_bundle_is_dispatched_at_the_peer_and_completes() {
        let (mut sender, mut receiver) = pair(Verdict::Whole, Qbcl::builder()).await;

        assert!(matches!(
            sender
                .forward(&receiver, &id(1), bundle(5000))
                .await
                .unwrap(),
            ForwardBundleResult::Accepted
        ));
        assert_eq!(
            receiver.next().await,
            Event::Dispatched {
                peer_node: Some(node_id(SENDER)),
                peer_addr: Some(sender.addr()),
                bundle: Some(bundle(5000)),
                acceptance: Acceptance::Accepted,
            }
        );
        assert_eq!(
            sender.next().await,
            Event::Outcome(id(1), TransferOutcome::Completed)
        );
    }

    // Both ends report the other as a peer, with the Node ID from its
    // certificate at the address it listens on.
    #[tokio::test]
    async fn a_session_adds_each_end_as_a_peer_of_the_other() {
        let (mut sender, mut receiver) = pair(Verdict::Whole, Qbcl::builder()).await;

        sender
            .cla
            .connect(receiver.cla.local_addr().unwrap())
            .await
            .unwrap();
        assert_eq!(
            sender.events.recv().await,
            Some(Event::AddPeer(receiver.addr(), vec![node_id(RECEIVER)]))
        );
        assert_eq!(
            receiver.events.recv().await,
            Some(Event::AddPeer(sender.addr(), vec![node_id(SENDER)]))
        );
    }

    #[tokio::test]
    async fn a_closed_session_removes_the_peer() {
        let (sender, mut receiver) = pair(Verdict::Whole, Qbcl::builder()).await;

        sender
            .cla
            .connect(receiver.cla.local_addr().unwrap())
            .await
            .unwrap();
        sender.cla.on_unregister().await;
        assert_eq!(receiver.next().await, Event::RemovePeer(sender.addr()));
    }

    #[tokio::test]
    async fn forwards_to_one_peer_share_a_session() {
        let (mut sender, receiver) = pair(Verdict::Whole, Qbcl::builder()).await;

        let (first_id, second_id) = (id(1), id(2));
        let (first, second) = tokio::join!(
            sender.forward(&receiver, &first_id, bundle(100)),
            sender.forward(&receiver, &second_id, bundle(100)),
        );
        assert!(matches!(first, Ok(ForwardBundleResult::Accepted)));
        assert!(matches!(second, Ok(ForwardBundleResult::Accepted)));
        // Shutting down waits for every task, so every peer the sender's
        // sessions added has been reported by then.
        sender.cla.on_unregister().await;
        let mut added = Vec::new();
        while let Ok(event) = sender.events.try_recv() {
            if let Event::AddPeer(addr, _) = event {
                added.push(addr);
            }
        }
        assert_eq!(added, [receiver.addr()]);
    }

    // A BPA may accept a bundle, a duplicate say, before it has pulled all
    // of it. The sender must still see the transfer complete.
    #[tokio::test]
    async fn a_bundle_accepted_before_its_end_still_completes_at_the_sender() {
        let (mut sender, mut receiver) =
            pair(Verdict::AfterFirst(Acceptance::Accepted), Qbcl::builder()).await;

        sender
            .forward(&receiver, &id(1), bundle(LARGER_THAN_THE_STREAM_WINDOW))
            .await
            .unwrap();
        assert!(matches!(receiver.next().await, Event::Dispatched { .. }));
        assert_eq!(
            sender.next().await,
            Event::Outcome(id(1), TransferOutcome::Completed)
        );
    }

    // The code a peer cancelled a forward with. The bundles these tests
    // send are larger than the stream window, so the sender is still
    // writing when the cancellation arrives.
    fn cancelled_by_peer(result: cla::Result<ForwardBundleResult>) -> ErrorCode {
        let Err(cla::Error::Internal(e)) = result else {
            panic!("expected the peer to cancel the forward");
        };
        match e.downcast_ref::<reliable::Error>() {
            Some(reliable::Error::Cancelled(code)) => *code,
            _ => panic!("expected a cancellation, got {e}"),
        }
    }

    #[tokio::test]
    async fn a_bundle_refused_while_it_arrives_is_cancelled() {
        let (sender, mut receiver) =
            pair(Verdict::AfterFirst(Acceptance::Refused), Qbcl::builder()).await;

        let result = sender
            .forward(&receiver, &id(1), bundle(LARGER_THAN_THE_STREAM_WINDOW))
            .await;
        assert_eq!(cancelled_by_peer(result), ErrorCode::TransferCancelled);
        assert!(matches!(receiver.next().await, Event::Dispatched { .. }));
    }

    // The wrapped form declares the length, so the bundle is cancelled on
    // its head alone.
    #[tokio::test]
    async fn a_bundle_over_the_receivers_limit_is_cancelled_with_bundle_too_large() {
        let limit = NonZeroU64::new(1000).unwrap();
        let (sender, mut receiver) =
            pair(Verdict::Whole, Qbcl::builder().max_bundle_size(limit)).await;

        let result = sender
            .forward(&receiver, &id(1), bundle(LARGER_THAN_THE_STREAM_WINDOW))
            .await;
        assert_eq!(cancelled_by_peer(result), ErrorCode::BundleTooLarge);
        assert!(matches!(
            receiver.next().await,
            Event::Dispatched {
                bundle: None,
                acceptance: Acceptance::Refused,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_bpv6_bundle_is_cancelled_with_unsupported_bundle_version() {
        let (sender, mut receiver) = pair(Verdict::Whole, Qbcl::builder()).await;
        let bpv6: Bytes = [0x06]
            .into_iter()
            .chain(bundle(LARGER_THAN_THE_STREAM_WINDOW - 1))
            .collect();

        let result = sender.forward(&receiver, &id(1), bpv6).await;
        assert_eq!(
            cancelled_by_peer(result),
            ErrorCode::UnsupportedBundleVersion
        );
        assert!(matches!(
            receiver.next().await,
            Event::Dispatched {
                bundle: None,
                acceptance: Acceptance::Refused,
                ..
            }
        ));
    }

    // A producer that hands over one segment, then goes away.
    struct Truncated(bool);

    #[async_trait]
    impl Receiver<Segment> for Truncated {
        async fn recv(&mut self) -> core::result::Result<Segment, RecvError> {
            if core::mem::replace(&mut self.0, true) {
                Err(RecvError)
            } else {
                Ok(Segment::Next(bundle(100)))
            }
        }
    }

    #[tokio::test]
    async fn a_truncated_bundle_fails_the_forward_and_is_not_delivered() {
        let (sender, mut receiver) = pair(Verdict::Whole, Qbcl::builder()).await;

        let result = sender
            .cla
            .forward(None, &receiver.addr(), &id(1), 200, &mut Truncated(false))
            .await;
        assert!(matches!(result, Err(cla::Error::StreamCancelled)));
        assert!(matches!(
            receiver.next().await,
            Event::Dispatched {
                bundle: None,
                acceptance: Acceptance::Refused,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_producer_short_of_the_declared_length_fails_the_forward() {
        let (sender, receiver) = pair(Verdict::Whole, Qbcl::builder()).await;

        let result = sender
            .cla
            .forward(None, &receiver.addr(), &id(1), 200, &mut bundle(100))
            .await;
        assert!(matches!(
            result,
            Err(cla::Error::PayloadUnderrun {
                size: 100,
                expected: 200
            })
        ));
    }

    // Section 4.2: a dial whose peer lacks the configured Node ID fails its
    // handshake, so there is no neighbour to forward to.
    #[tokio::test]
    async fn a_peer_without_its_configured_node_id_is_no_neighbour() {
        let ca = Ca::new("trusted");
        let receiver = Node::new(&ca, RECEIVER, Verdict::Whole, Qbcl::builder()).await;
        let builder = Qbcl::builder().peer(
            receiver.cla.local_addr().unwrap(),
            node_id("ipn:977000.3.0"),
        );
        let sender = Node::new(&ca, SENDER, Verdict::Whole, builder).await;

        assert!(matches!(
            sender
                .forward(&receiver, &id(1), bundle(100))
                .await
                .unwrap(),
            ForwardBundleResult::NoNeighbour
        ));
    }

    #[tokio::test]
    async fn an_address_of_another_type_is_no_neighbour() {
        let (sender, _) = pair(Verdict::Whole, Qbcl::builder()).await;
        let tcp = ClaAddress::Tcp(sender.cla.local_addr().unwrap());

        let result = sender
            .cla
            .forward(None, &tcp, &id(1), 100, &mut bundle(100))
            .await;
        assert!(matches!(result, Ok(ForwardBundleResult::NoNeighbour)));
    }

    #[tokio::test]
    async fn the_adapter_declares_quic_addresses_and_its_limit() {
        let ca = Ca::new("trusted");
        let limit = NonZeroU64::new(1000).unwrap();
        let cla = Qbcl::builder()
            .listen("127.0.0.1:0".parse().unwrap())
            .tls(ca.tls(ca.identity("node", Some(bundle_eids(&[SENDER])))))
            .max_bundle_size(limit)
            .build()
            .unwrap();
        assert_eq!(
            cla.cla_init(),
            ClaInit {
                address_type: Some(ClaAddressType::Quic),
                lane_count: None,
                max_bundle_size: Some(limit),
            }
        );
    }

    #[tokio::test]
    async fn building_needs_a_listen_address_and_tls() {
        let ca = Ca::new("trusted");
        let tls = || ca.tls(ca.identity("node", Some(bundle_eids(&[SENDER]))));
        assert!(matches!(
            Qbcl::builder().tls(tls()).build(),
            Err(Error::NoListenAddress)
        ));
        assert!(matches!(
            Qbcl::builder()
                .listen("127.0.0.1:0".parse().unwrap())
                .build(),
            Err(Error::NoTls)
        ));
    }
}
