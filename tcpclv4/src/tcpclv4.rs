//! [`Tcpclv4`]: the TCPCL entity — the CLA that registers with a BPA.

use core::num::{NonZeroU32, NonZeroUsize};
use std::net::TcpListener;
use std::sync::Mutex;

use hardy_bpa::{
    async_trait,
    cla::{
        self, Cla, ClaAddress, ClaAddressType, ClaInit, ForwardBundleResult, Sink, TransferOutcome,
    },
};
use hardy_bpv7::bundle::Id;

use super::*;

use crate::error::Error;

/// Registration-time state from BPA.
struct Inner {
    sink: Arc<dyn Sink>,
    node_ids: Arc<[NodeId]>,
    // The advertised transfer MRU: the configured value clamped to the
    // BPA's dispatch size cap, so a peer is never invited to send a
    // transfer the BPA would deterministically refuse.
    transfer_mru: NonZeroU64,
    // The advertised segment MRU, clamped to the transfer MRU above: a
    // single segment larger than any completable transfer would be an
    // invitation the session could only answer with XFER_REFUSE.
    segment_mru: NonZeroU64,
}

/// TCPCLv4 Convergence Layer Adapter (RFC 9174).
///
/// Manages TCP connections to peer nodes, including listener and connector
/// roles, optional TLS, keepalive, and transfer segmentation. Implements
/// the BPA CLA trait so it can be registered with a BPA instance.
pub struct Tcpclv4 {
    // Builder inputs
    contact_timeout: ContactTimeout,
    keepalive_interval: KeepaliveInterval,
    listeners: Mutex<Vec<TcpListener>>,
    connection_rate_limit: NonZeroU32,
    segment_mru: NonZeroU64,
    transfer_mru: NonZeroU64,

    // Computed at construction
    tls: Option<Arc<tls::Tls>>,
    registry: Arc<connection::ConnectionRegistry>,
    session_cancel_token: tokio_util::sync::CancellationToken,

    // Late-init from registration (single atomic)
    inner: Once<Inner>,

    // Task management
    tasks: Arc<hardy_async::TaskPool>,
}

impl Tcpclv4 {
    /// Starts building a [`Tcpclv4`] with the defaults documented on the builder's methods.
    pub fn builder() -> builder::Tcpclv4Builder {
        builder::Tcpclv4Builder::new()
    }

    // Assembles the entity from the builder's inputs and the loaded TLS
    // material; the runtime skeleton (registry, cancellation token, task
    // pool) is constructed here, next to the fields it initialises.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        contact_timeout: ContactTimeout,
        keepalive_interval: KeepaliveInterval,
        listeners: Vec<TcpListener>,
        connection_rate_limit: NonZeroU32,
        segment_mru: NonZeroU64,
        transfer_mru: NonZeroU64,
        max_idle_connections: usize,
        max_outstanding_transfers: NonZeroUsize,
        tls: Option<Arc<tls::Tls>>,
    ) -> Self {
        Self {
            contact_timeout,
            keepalive_interval,
            listeners: Mutex::new(listeners),
            connection_rate_limit,
            segment_mru,
            transfer_mru,
            tls,
            registry: Arc::new(connection::ConnectionRegistry::new(
                max_idle_connections,
                max_outstanding_transfers,
            )),
            session_cancel_token: tokio_util::sync::CancellationToken::new(),
            inner: Once::new(),
            tasks: Arc::new(hardy_async::TaskPool::new()),
        }
    }

    /// The entity's set-once registration declarations, for the embedder
    /// to pass to `register_cla`: a TCP address type and the configured
    /// transfer MRU as the declared receive limit, which the BPA folds
    /// into the effective `max_bundle_size` reported back at
    /// registration. The MRU here is the pre-registration declaration:
    /// once registered, sessions advertise the effective (possibly
    /// smaller) clamped MRU.
    pub fn cla_init(&self) -> ClaInit {
        ClaInit {
            address_type: Some(ClaAddressType::Tcp),
            lane_count: None,
            max_bundle_size: Some(self.transfer_mru),
        }
    }

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        if let Some(inner) = self.inner.get() {
            inner.sink.unregister().await;
        }
    }

    /// Initiates a TCP connection to a remote peer (RFC 9174 Section 3).
    ///
    /// Retries up to 5 times on timeout before returning an error. Returns [`cla::Error::Disconnected`] before registration and once the entity has unregistered.
    pub async fn connect(&self, remote_addr: &SocketAddr) -> cla::Result<()> {
        let ctx = self.connection_context().ok_or_else(|| {
            error!("connect called before on_register!");
            cla::Error::Disconnected
        })?;
        if self.session_cancel_token.is_cancelled() {
            return Err(cla::Error::Disconnected);
        }

        for _ in 0..5 {
            let conn = connection::connect::Connector {
                tasks: self.tasks.clone(),
                ctx: ctx.clone(),
            };
            match conn.connect(remote_addr).await {
                Ok(()) => return Ok(()),
                Err(session::Error::PeerTimeout) => {}
                Err(e) => {
                    return Err(cla::Error::Internal(Error::from(e).into()));
                }
            }
        }
        Err(cla::Error::Internal(
            Error::from(session::Error::PeerTimeout).into(),
        ))
    }

    /// Waits until this entity holds no session to `remote_addr`, dialled or accepted.
    ///
    /// Returns at once when there is none. An embedder that keeps a peer connected waits on this after [`connect`](Self::connect) succeeds, then dials again.
    pub async fn disconnected(&self, remote_addr: &SocketAddr) {
        self.registry.sessions_closed(remote_addr).await
    }

    /// Creates a ConnectionContext for use in connect/forward operations.
    fn connection_context(&self) -> Option<connection::context::ConnectionContext> {
        let inner = self.inner.get()?;

        Some(connection::context::ConnectionContext {
            contact_timeout: self.contact_timeout,
            keepalive_interval: self.keepalive_interval,
            segment_mru: inner.segment_mru,
            transfer_mru: inner.transfer_mru,
            node_ids: inner.node_ids.clone(),
            sink: inner.sink.clone(),
            registry: self.registry.clone(),
            tls: self.tls.clone(),
            session_cancel_token: self.session_cancel_token.clone(),
            task_cancel_token: self.tasks.cancel_token().clone(),
        })
    }

    // Registration consumes the entity's sink slot and its bound listener
    // sockets, so it succeeds exactly once.
    fn start_listeners(&self) {
        let listeners = std::mem::take(
            &mut *self
                .listeners
                .lock()
                .trace_expect("listener mutex poisoned"),
        );
        for listener in listeners {
            let ctx = self
                .connection_context()
                .trace_expect("start_listeners called before registration");

            let acceptor = connection::listen::Listener {
                connection_rate_limit: self.connection_rate_limit,
                ctx,
            };
            self.tasks
                .spawn(acceptor.listen(self.tasks.clone(), listener));
        }
    }
}

#[async_trait]
impl Cla for Tcpclv4 {
    #[cfg_attr(feature = "instrument", instrument(skip(self, sink)))]
    async fn on_register(
        &self,
        sink: Box<dyn Sink>,
        node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        // The effective cap already folds this entity's declared transfer
        // MRU (`Tcpclv4::cla_init`) with the BPA's own; the min is kept
        // as a cheap guard against a registrar that ignored the declaration.
        // No negotiated cap leaves the configured MRU alone.
        let transfer_mru =
            max_bundle_size.map_or(self.transfer_mru, |cap| self.transfer_mru.min(cap));
        if transfer_mru < self.transfer_mru {
            info!(
                "Transfer MRU clamped from {} to the BPA's max bundle size {transfer_mru}",
                self.transfer_mru
            );
        }

        // A single segment can never usefully exceed the transfer it
        // belongs to, so the advertised segment MRU folds the same way.
        let segment_mru = self.segment_mru.min(transfer_mru);

        // Registration consumes the entity's sink slot and its bound
        // listener sockets, so it succeeds exactly once
        let mut first_registration = false;
        self.inner.call_once(|| {
            first_registration = true;
            Inner {
                sink: sink.into(),
                node_ids: node_ids.into(),
                transfer_mru,
                segment_mru,
            }
        });
        if !first_registration {
            warn!("Registration refused: {}", Error::AlreadyRegistered);
            return;
        }

        self.start_listeners();
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn on_unregister(&self) {
        // Cancel sessions first so they exit promptly when channels close
        self.session_cancel_token.cancel();

        // Shutdown all pooled connections (drops tx senders)
        self.registry.shutdown();

        // Wait for all session tasks to complete
        self.tasks.shutdown().await;
    }

    // INTERIM BUFFERING: the transfer is resolved out-of-band by a spawned
    // task that retries across pooled connections, so the whole bundle is
    // assembled in memory via `stream::buffer_stream` before the offer is
    // answered. This is a deliberate stepping stone toward the full
    // streaming pipeline (a native implementation would stream segments
    // straight into XFER_SEGMENT frames, sized from `total_len`); see
    // bpa/docs/streaming_pipeline_design.md.
    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    async fn forward(
        &self,
        _lane: Option<u32>,
        cla_addr: &ClaAddress,
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<cla::Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        let ctx = self.connection_context().ok_or_else(|| {
            error!("forward called before on_register!");
            cla::Error::Disconnected
        })?;

        let ClaAddress::Tcp(remote_addr) = cla_addr else {
            return Ok(ForwardBundleResult::NoNeighbour);
        };

        let bundle = hardy_bpa::stream::buffer_stream(stream, total_len).await?;

        debug!("Forwarding bundle to TCPCLv4 peer at {remote_addr}");

        // Take ownership of the transfer and resolve it out-of-band: the
        // session-side transmit-and-acknowledge cycle costs at least one
        // round trip, and answering the offer first keeps the BPA's egress
        // flowing while transfers overlap across pooled connections. The
        // permit bounds accepted-but-unresolved transfers per peer; awaiting
        // it withholds the verdict, which is the flow control back to the
        // BPA.
        let permits = ctx.registry.transfer_permits(*remote_addr);
        let permit = permits
            .acquire_owned()
            .await
            .trace_expect("Transfer permit semaphore closed");

        let tasks = self.tasks.clone();
        let bundle_id = bundle_id.clone();
        let remote_addr = *remote_addr;
        self.tasks.spawn(async move {
            let _permit = permit;

            let outcome = transmit(tasks, &ctx, remote_addr, bundle).await;
            if let Err(e) = ctx.sink.transfer_outcome(&bundle_id, outcome).await {
                debug!("Failed to report transfer outcome: {e}");
            }
        });

        Ok(ForwardBundleResult::Accepted)
    }
}

// Peers can close at random times, so a transfer is retried this many
// times before falling back to a last try on a busy session.
const DIAL_ATTEMPTS: usize = 5;

// Transmit a bundle to `remote_addr` over a pooled session, dialing new
// connections as the pool allows (only to an address a dialled session has
// reached), and return the terminal outcome of the transfer: `Completed`
// only once the peer has fully acknowledged it.
async fn transmit(
    tasks: Arc<hardy_async::TaskPool>,
    ctx: &connection::context::ConnectionContext,
    remote_addr: SocketAddr,
    mut bundle: hardy_bpa::Bytes,
) -> TransferOutcome {
    for _ in 0..DIAL_ATTEMPTS {
        // Use a pooled session, dialing a new connection when the
        // pool has capacity and no session is free
        bundle = match ctx
            .registry
            .forward(&remote_addr, bundle, connection::OnBusy::Dial)
            .await
        {
            Ok(r) => {
                debug!("Bundle forwarded successfully using existing connection");
                return r;
            }
            Err(bundle) => {
                debug!("No free connections, will attempt to create new one");
                bundle
            }
        };

        // An address no dialled session has reached belongs to a pool of
        // accepted sessions, the peer's source address: there is nothing to
        // dial, so queue on whatever session remains
        let Some(dial_lock) = ctx.registry.dial_lock(&remote_addr) else {
            return ctx
                .registry
                .forward(&remote_addr, bundle, connection::OnBusy::Queue)
                .await
                .unwrap_or(TransferOutcome::Failed);
        };

        // One dial at a time per peer: concurrent forwards coalesce here
        // rather than racing parallel dials, and the pool is re-checked
        // under the lock — the previous holder's session may already be
        // registered
        let _dialing = dial_lock.lock().await;
        bundle = match ctx
            .registry
            .forward(&remote_addr, bundle, connection::OnBusy::Dial)
            .await
        {
            Ok(r) => return r,
            Err(bundle) => bundle,
        };

        // Do a new active connect
        let conn = connection::connect::Connector {
            tasks: tasks.clone(),
            ctx: ctx.clone(),
        };
        match conn.connect(&remote_addr).await {
            Ok(()) => {}
            Err(session::Error::PeerTimeout) if !ctx.registry.has_sessions(&remote_addr) => {
                // Nothing to fall back to: keep dialing
            }
            Err(e) => {
                // The dial failed but a session may be up: fall back to
                // queueing on it rather than stalling the forward behind
                // further dial attempts. Silently dropped SYNs (dial
                // timeout) are the norm for firewalled peers with
                // asymmetric reachability that hold a session open without
                // being able to accept another.
                debug!("Dial to {remote_addr} failed: {e:?}; falling back to a busy session");
                return ctx
                    .registry
                    .forward(&remote_addr, bundle, connection::OnBusy::Queue)
                    .await
                    .unwrap_or(TransferOutcome::Failed);
            }
        }
    }

    // Repeated dial timeouts: last try on a busy session before
    // giving up on the transfer
    ctx.registry
        .forward(&remote_addr, bundle, connection::OnBusy::Queue)
        .await
        .unwrap_or(TransferOutcome::Failed)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use hardy_bpv7::eid::IpnNodeId;

    use super::*;

    struct MockSink;

    #[async_trait]
    impl Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&ClaAddress>,
            _stream: &mut dyn hardy_bpa::stream::Receiver<cla::Segment>,
        ) -> cla::Result<cla::Acceptance> {
            Ok(cla::Acceptance::Accepted)
        }

        async fn add_peer(&self, _cla_addr: ClaAddress, _node_ids: &[NodeId]) -> cla::Result<bool> {
            Ok(true)
        }

        async fn remove_peer(&self, _cla_addr: &ClaAddress) -> cla::Result<bool> {
            Ok(true)
        }

        async fn transfer_outcome(
            &self,
            _bundle_id: &Id,
            _outcome: TransferOutcome,
        ) -> cla::Result<()> {
            Ok(())
        }
    }

    // What a `RecordingSink` saw, in order
    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Add(ClaAddress),
        Remove(ClaAddress),
        Dispatch(Option<ClaAddress>, hardy_bpa::Bytes),
    }

    // A sink that keeps the BPA's adjacency claims (one per address; a
    // second add_peer at a claimed address is refused) and reports every
    // claim change and delivered bundle on a channel, in order.
    struct RecordingSink {
        claims: Mutex<HashMap<ClaAddress, Vec<NodeId>>>,
        events: tokio::sync::mpsc::UnboundedSender<Event>,
    }

    impl RecordingSink {
        fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<Event>) {
            let (events, rx) = tokio::sync::mpsc::unbounded_channel();
            (
                Self {
                    claims: Mutex::new(HashMap::new()),
                    events,
                },
                rx,
            )
        }
    }

    #[async_trait]
    impl Sink for RecordingSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            peer_addr: Option<&ClaAddress>,
            stream: &mut dyn hardy_bpa::stream::Receiver<cla::Segment>,
        ) -> cla::Result<cla::Acceptance> {
            let bundle = hardy_bpa::stream::concat_stream(stream, usize::MAX)
                .await
                .unwrap();
            _ = self
                .events
                .send(Event::Dispatch(peer_addr.cloned(), bundle));
            Ok(cla::Acceptance::Accepted)
        }

        async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> cla::Result<bool> {
            let mut claims = self.claims.lock().unwrap();
            if claims.contains_key(&cla_addr) {
                return Ok(false);
            }
            claims.insert(cla_addr.clone(), node_ids.to_vec());
            _ = self.events.send(Event::Add(cla_addr));
            Ok(true)
        }

        async fn remove_peer(&self, cla_addr: &ClaAddress) -> cla::Result<bool> {
            let removed = self.claims.lock().unwrap().remove(cla_addr).is_some();
            if removed {
                _ = self.events.send(Event::Remove(cla_addr.clone()));
            }
            Ok(removed)
        }

        async fn transfer_outcome(
            &self,
            _bundle_id: &Id,
            _outcome: TransferOutcome,
        ) -> cla::Result<()> {
            Ok(())
        }
    }

    fn ipn(node_number: u32) -> NodeId {
        NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number,
        })
    }

    // A registered entity for `node` listening on an ephemeral loopback
    // port, with its sink's claims, its event stream and its listen address
    async fn entity(
        node: u32,
    ) -> (
        Tcpclv4,
        Arc<RecordingSink>,
        tokio::sync::mpsc::UnboundedReceiver<Event>,
        SocketAddr,
    ) {
        let cla = Tcpclv4::builder()
            .listen("127.0.0.1:0".parse().unwrap())
            .build()
            .unwrap();
        let listen = cla.listeners.lock().unwrap()[0].local_addr().unwrap();
        let (sink, events) = RecordingSink::new();
        let sink = Arc::new(sink);
        cla.on_register(Box::new(ArcSink(sink.clone())), &[ipn(node)], None)
            .await;
        (cla, sink, events, listen)
    }

    // Shares a `RecordingSink` between the entity and the test
    struct ArcSink(Arc<RecordingSink>);

    #[async_trait]
    impl Sink for ArcSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            peer_node: Option<&NodeId>,
            peer_addr: Option<&ClaAddress>,
            stream: &mut dyn hardy_bpa::stream::Receiver<cla::Segment>,
        ) -> cla::Result<cla::Acceptance> {
            self.0.dispatch(peer_node, peer_addr, stream).await
        }

        async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> cla::Result<bool> {
            self.0.add_peer(cla_addr, node_ids).await
        }

        async fn remove_peer(&self, cla_addr: &ClaAddress) -> cla::Result<bool> {
            self.0.remove_peer(cla_addr).await
        }

        async fn transfer_outcome(
            &self,
            bundle_id: &Id,
            outcome: TransferOutcome,
        ) -> cla::Result<()> {
            self.0.transfer_outcome(bundle_id, outcome).await
        }
    }

    async fn next_event(events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
        // the timeout only bounds a regression; the event follows at once
        tokio::time::timeout(tokio::time::Duration::from_secs(10), events.recv())
            .await
            .expect("no sink event")
            .expect("sink dropped")
    }

    // Forward `payload` from `cla` to `to` and wait for the peer's sink to
    // deliver it: returns the ingress address the peer recorded
    async fn deliver(
        cla: &Tcpclv4,
        to: SocketAddr,
        payload: &'static [u8],
        peer_events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    ) -> Option<ClaAddress> {
        let mut bundle = hardy_bpa::Bytes::from_static(payload);
        let result = cla
            .forward(
                None,
                &ClaAddress::Tcp(to),
                &Id::default(),
                payload.len() as u64,
                &mut bundle,
            )
            .await
            .unwrap();
        assert!(matches!(result, ForwardBundleResult::Accepted));
        match next_event(peer_events).await {
            Event::Dispatch(peer_addr, bundle) if bundle == payload => peer_addr,
            e => panic!("expected the delivery of {payload:?}, got {e:?}"),
        }
    }

    // Both entities dial each other, the peer first: the inbound pool at A
    // gets the adjacency for B's node, then A's own dial takes it over, so
    // A forwards to B over the pool that can grow. The inbound session
    // still delivers what B sends.
    #[tokio::test]
    async fn own_dial_takes_the_adjacency_from_the_peers_session() {
        let (a, a_sink, mut a_events, a_listen) = entity(1).await;
        let (b, _b_sink, mut b_events, b_listen) = entity(2).await;

        b.connect(&a_listen).await.unwrap();
        let Event::Add(ClaAddress::Tcp(inbound)) = next_event(&mut a_events).await else {
            panic!("expected an adjacency for B's session");
        };
        assert_ne!(inbound, b_listen);
        assert_eq!(
            next_event(&mut b_events).await,
            Event::Add(ClaAddress::Tcp(a_listen))
        );

        a.connect(&b_listen).await.unwrap();
        assert_eq!(
            next_event(&mut a_events).await,
            Event::Add(ClaAddress::Tcp(b_listen))
        );
        assert_eq!(
            next_event(&mut a_events).await,
            Event::Remove(ClaAddress::Tcp(inbound))
        );
        assert_eq!(
            *a_sink.claims.lock().unwrap(),
            HashMap::from([(ClaAddress::Tcp(b_listen), vec![ipn(2)])])
        );

        // B's bundles still arrive over the session B dialled
        assert_eq!(
            deliver(&b, a_listen, b"from B", &mut a_events).await,
            Some(ClaAddress::Tcp(inbound))
        );

        a.on_unregister().await;
        b.on_unregister().await;
    }

    // Both entities dial each other, this one first: the peer's later
    // session joins a pool that withholds its adjacency, so the BPA keeps
    // forwarding over the dialled pool alone, and the withheld session
    // still delivers what the peer sends.
    #[tokio::test]
    async fn peers_session_withholds_its_adjacency_from_an_own_dial() {
        let (a, a_sink, mut a_events, a_listen) = entity(1).await;
        let (b, _b_sink, mut b_events, b_listen) = entity(2).await;

        a.connect(&b_listen).await.unwrap();
        assert_eq!(
            next_event(&mut a_events).await,
            Event::Add(ClaAddress::Tcp(b_listen))
        );
        // B advertises A's session until B's own dial takes over
        let Event::Add(ClaAddress::Tcp(at_b)) = next_event(&mut b_events).await else {
            panic!("expected an adjacency for A's session at B");
        };

        b.connect(&a_listen).await.unwrap();
        assert_eq!(
            next_event(&mut b_events).await,
            Event::Add(ClaAddress::Tcp(a_listen))
        );
        assert_eq!(
            next_event(&mut b_events).await,
            Event::Remove(ClaAddress::Tcp(at_b))
        );

        // The delivery is the barrier: B's session registered at A before
        // its first bundle reached A's sink, with no claim change between
        let Some(ClaAddress::Tcp(inbound)) = deliver(&b, a_listen, b"from B", &mut a_events).await
        else {
            panic!("expected B's session's address");
        };
        assert_ne!(inbound, b_listen);
        assert_eq!(
            *a_sink.claims.lock().unwrap(),
            HashMap::from([(ClaAddress::Tcp(b_listen), vec![ipn(2)])])
        );

        a.on_unregister().await;
        b.on_unregister().await;
    }

    // The transfer MRU sessions advertise is the configured value folded
    // with the cap negotiated at registration: a smaller negotiated cap
    // clamps it, a larger or absent one leaves it alone.
    #[tokio::test]
    async fn registration_folds_transfer_mru_with_negotiated_cap() {
        let configured = NonZeroU64::new(1024 * 1024).unwrap();
        let smaller = NonZeroU64::new(configured.get() / 2).unwrap();
        let larger = NonZeroU64::new(configured.get() * 2).unwrap();

        let cla = Tcpclv4::builder().transfer_mru(configured).build().unwrap();
        cla.on_register(Box::new(MockSink), &[], Some(smaller))
            .await;
        assert_eq!(cla.inner.get().unwrap().transfer_mru, smaller);

        let cla = Tcpclv4::builder().transfer_mru(configured).build().unwrap();
        cla.on_register(Box::new(MockSink), &[], Some(larger)).await;
        assert_eq!(cla.inner.get().unwrap().transfer_mru, configured);

        let cla = Tcpclv4::builder().transfer_mru(configured).build().unwrap();
        cla.on_register(Box::new(MockSink), &[], None).await;
        assert_eq!(cla.inner.get().unwrap().transfer_mru, configured);
    }

    // The segment MRU sessions advertise is clamped to the folded transfer
    // MRU: a SESS_INIT never invites a single segment larger than any
    // completable transfer.
    #[tokio::test]
    async fn registration_clamps_segment_mru_to_folded_transfer_mru() {
        let segment = NonZeroU64::new(16 * 1024).unwrap();
        let transfer = NonZeroU64::new(1024 * 1024).unwrap();
        let below_segment = NonZeroU64::new(segment.get() / 2).unwrap();

        // A negotiated cap below the segment MRU drags the segment MRU
        // down along with the transfer MRU.
        let cla = Tcpclv4::builder()
            .segment_mru(segment)
            .transfer_mru(transfer)
            .build()
            .unwrap();
        cla.on_register(Box::new(MockSink), &[], Some(below_segment))
            .await;
        let inner = cla.inner.get().unwrap();
        assert_eq!(inner.transfer_mru, below_segment);
        assert_eq!(inner.segment_mru, below_segment);

        // A transfer MRU above the segment MRU leaves it alone.
        let cla = Tcpclv4::builder()
            .segment_mru(segment)
            .transfer_mru(transfer)
            .build()
            .unwrap();
        cla.on_register(Box::new(MockSink), &[], None).await;
        assert_eq!(cla.inner.get().unwrap().segment_mru, segment);
    }

    // A forward whose pool has gone dials only an address a dialled
    // session has reached: the address of a pool of accepted sessions is
    // the peer's source port, so the transfer fails back to the BPA rather
    // than dialling it. A listener stands in for that port, and
    // `transmit` returning is the barrier after which a dial would be
    // sitting in its accept queue.
    #[tokio::test]
    async fn transmit_never_dials_an_address_no_dialled_session_reached() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();

        let cla = Tcpclv4::builder()
            .contact_timeout(ContactTimeout::new(1).unwrap())
            .build()
            .unwrap();
        cla.on_register(Box::new(MockSink), &[], None).await;
        let ctx = cla.connection_context().unwrap();

        let outcome = transmit(
            cla.tasks.clone(),
            &ctx,
            address,
            hardy_bpa::Bytes::from_static(b"bundle"),
        )
        .await;
        assert_eq!(outcome, TransferOutcome::Failed);
        assert_eq!(
            listener.accept().map_err(|e| e.kind()).err(),
            Some(std::io::ErrorKind::WouldBlock),
            "transmit dialled an address no dialled session had reached"
        );
    }

    // RFC 9174 Section 2.1: an entity may support zero or more passive
    // listening elements, each bound during build().
    #[test]
    fn listen_adds_bound_passive_listening_elements() {
        let cla = Tcpclv4::builder()
            .build()
            .expect("a dial-only plaintext build cannot fail");
        assert!(cla.listeners.lock().unwrap().is_empty());
        assert!(cla.tls.is_none());

        let cla = Tcpclv4::builder()
            .listen("[::1]:0".parse().unwrap())
            .listen("[::1]:0".parse().unwrap())
            .build()
            .unwrap();
        let listeners = cla.listeners.lock().unwrap();
        assert_eq!(listeners.len(), 2);
        for listener in listeners.iter() {
            assert_ne!(listener.local_addr().unwrap().port(), 0);
        }
    }

    // A socket that cannot be bound fails the build instead of a
    // background accept task.
    #[test]
    fn bind_failures_surface_at_build() {
        let occupied = std::net::TcpListener::bind("[::1]:0").unwrap();
        let address = occupied.local_addr().unwrap();
        let Err(err) = Tcpclv4::builder().listen(address).build() else {
            panic!("binding an occupied port must fail");
        };
        assert!(matches!(err, error::Error::BindListener { .. }));
    }

    // Requiring TLS without an identity leaves listeners nothing they
    // could serve, rejected before any socket is bound.
    #[test]
    fn required_tls_listeners_need_an_identity() {
        let Err(err) = Tcpclv4::builder()
            .listen("[::1]:0".parse().unwrap())
            .tls(
                tls::Tls::builder()
                    .dangerous()
                    .insecure_skip_verify()
                    .required(true)
                    .build()
                    .unwrap(),
            )
            .build()
        else {
            panic!("listeners under identity-less required TLS must fail");
        };
        assert!(matches!(err, error::Error::RequiredTlsWithoutIdentity));
    }

    #[test]
    fn no_keepalive_disables_keepalives() {
        let cla = Tcpclv4::builder().no_keepalive().build().unwrap();
        assert!(cla.keepalive_interval.is_disabled());
    }

    // RFC 9174 Section 4.7: the negotiated keepalive is the minimum of
    // the two proposals; disabled from either side wins.
    #[test]
    fn keepalive_negotiation_is_a_minimum_where_disabled_wins() {
        assert_eq!(KeepaliveInterval::new(30).negotiate(60).get(), 30);
        assert_eq!(KeepaliveInterval::new(60).negotiate(30).get(), 30);
        assert_eq!(KeepaliveInterval::new(45).negotiate(45).get(), 45);
        assert!(KeepaliveInterval::DISABLED.negotiate(60).is_disabled());
        assert!(KeepaliveInterval::new(60).negotiate(0).is_disabled());
    }

    // The insecure stage needs no files, so the Required policy is
    // observable without touching disk.
    #[test]
    fn required_material_lands_as_the_required_policy() {
        let cla = Tcpclv4::builder()
            .tls(
                tls::Tls::builder()
                    .dangerous()
                    .insecure_skip_verify()
                    .required(true)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let tls = cla.tls.as_ref().expect("TLS material must be present");
        assert!(tls.is_required());
    }
}
