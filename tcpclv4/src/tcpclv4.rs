//! [`Tcpclv4`]: the TCPCL entity, and the parts a BPA builds it from.

use core::{
    fmt::{self, Debug, Formatter},
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
};
use std::{
    net::{SocketAddr, TcpListener},
    sync::{Arc, Mutex},
};

use hardy_async::TaskPool;
use hardy_bpa::{
    Bytes, async_trait,
    cla::{self, Address, Cla, Context, ForwardOutcome, Sink, TransferOutcome},
    stream::{Receiver, buffer_stream},
};
use hardy_bpv7::{bundle::Id, eid::NodeId};
use tokio_util::sync::CancellationToken;
use trace_err::TraceErrResult;
use tracing::{debug, info};

#[cfg(feature = "instrument")]
use tracing::instrument;

use crate::{
    ContactTimeout, KeepaliveInterval,
    builder::Tcpclv4Builder,
    config::Config,
    connection::{
        self, ConnectionRegistry, connect::Connector, context::ConnectionContext, listen::Listener,
    },
    error::Error,
    session,
    tls::Tls,
};

// The MRUs the entity's sessions advertise: the configured values folded
// with the receive limit the BPA settled on.
//
// The settled limit already folds the transfer MRU the entity declared;
// taking the smaller again guards against a registrar that ignored the
// declaration, and no settled limit leaves the configured MRU alone. A
// single segment can never usefully exceed the transfer it belongs to, so
// the segment MRU folds against the result: a SESS_INIT never invites a
// segment larger than any completable transfer.
fn advertised_mrus(
    transfer_mru: NonZeroU64,
    segment_mru: NonZeroU64,
    max_bundle_size: Option<NonZeroU64>,
) -> (NonZeroU64, NonZeroU64) {
    let folded = max_bundle_size.map_or(transfer_mru, |cap| transfer_mru.min(cap));
    if folded < transfer_mru {
        info!("Transfer MRU clamped from {transfer_mru} to the BPA's receive limit {folded}");
    }
    (folded, segment_mru.min(folded))
}

// The validated settings `Tcpclv4Builder::build` hands the entity, as
// against the inputs the builder consumes itself: listen addresses
// become bound listeners, and the TLS material is loaded.
pub(crate) struct Settings {
    pub contact_timeout: ContactTimeout,
    pub keepalive_interval: KeepaliveInterval,
    pub connection_rate_limit: NonZeroU32,
    pub segment_mru: NonZeroU64,
    pub transfer_mru: NonZeroU64,
    pub max_idle_connections: usize,
    pub max_outstanding_transfers: NonZeroUsize,
}

/// TCPCLv4 Convergence Layer Adapter (RFC 9174).
///
/// Manages TCP connections to peer nodes, including listener and connector
/// roles, optional TLS, keepalive, and transfer segmentation. A BPA builds
/// one from the factory [`Tcpclv4::factory`] or
/// [`Tcpclv4Builder::build`] returns, and
/// owns it for as long as it stays attached.
pub struct Tcpclv4 {
    // Builder inputs
    contact_timeout: ContactTimeout,
    keepalive_interval: KeepaliveInterval,
    listeners: Mutex<Vec<TcpListener>>,
    connection_rate_limit: NonZeroU32,
    // The advertised segment MRU, clamped to the transfer MRU below: a
    // single segment larger than any completable transfer would be an
    // invitation the session could only answer with XFER_REFUSE.
    segment_mru: NonZeroU64,
    // The advertised transfer MRU: the configured value clamped to the
    // receive limit the BPA settled on, so a peer is never invited to
    // send a transfer the BPA would deterministically refuse.
    transfer_mru: NonZeroU64,
    // The listeners' bound addresses, kept for `Debug`: the sockets
    // themselves move to their tasks when the entity starts.
    listen_addresses: Vec<SocketAddr>,

    // Computed at construction
    tls: Option<Arc<Tls>>,
    registry: Arc<ConnectionRegistry>,
    session_cancel_token: CancellationToken,

    // The BPA side of the attachment
    sink: Arc<dyn Sink>,
    node_ids: Arc<[NodeId]>,

    // Task management
    tasks: Arc<TaskPool>,
}

impl Tcpclv4 {
    // Assembles the entity from the builder's validated settings, the
    // bound listening elements, and the context the BPA settled: the
    // body of the factory `Tcpclv4Builder::build` returns.
    pub(crate) fn new(
        settings: Settings,
        listeners: Vec<TcpListener>,
        tls: Option<Arc<Tls>>,
        cx: Context,
    ) -> Arc<Self> {
        let (transfer_mru, segment_mru) = advertised_mrus(
            settings.transfer_mru,
            settings.segment_mru,
            cx.terms.max_bundle_size,
        );

        // A bound socket always has a local address; one it cannot report
        // is left out of the Debug list rather than failing construction.
        let listen_addresses = listeners
            .iter()
            .filter_map(|listener| listener.local_addr().ok())
            .collect();

        Arc::new(Self {
            contact_timeout: settings.contact_timeout,
            keepalive_interval: settings.keepalive_interval,
            listeners: Mutex::new(listeners),
            connection_rate_limit: settings.connection_rate_limit,
            segment_mru,
            transfer_mru,
            listen_addresses,
            tls,
            registry: Arc::new(ConnectionRegistry::new(
                settings.max_idle_connections,
                settings.max_outstanding_transfers,
            )),
            session_cancel_token: CancellationToken::new(),
            sink: cx.sink,
            node_ids: Vec::from(cx.node_ids.as_ref()).into(),
            tasks: Arc::new(TaskPool::new()),
        })
    }

    /// Starts building a [`Tcpclv4`] with the defaults documented on the builder's methods.
    pub fn builder() -> Tcpclv4Builder {
        Tcpclv4Builder::new()
    }

    /// A factory for the entity `config` describes, binding its listeners
    /// and loading its TLS material now.
    ///
    /// The shorthand for `Tcpclv4Builder::from(config).build()`; take the
    /// builder itself to adjust a setting the configuration file does not
    /// carry.
    ///
    /// # Errors
    ///
    /// The errors of
    /// [`Tcpclv4Builder::build`].
    pub fn factory(config: Config) -> Result<impl cla::Factory<Component = Self>, Error> {
        Tcpclv4Builder::from(config).build()
    }

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        self.sink.unregister().await;
    }

    /// Initiates a TCP connection to a remote peer (RFC 9174 Section 3).
    ///
    /// Retries up to 5 times on timeout before returning an error.
    pub async fn connect(&self, remote_addr: &SocketAddr) -> cla::Result<()> {
        let ctx = self.connection_context();

        for _ in 0..5 {
            let conn = Connector {
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

    /// Creates a ConnectionContext for use in connect/forward operations.
    fn connection_context(&self) -> ConnectionContext {
        ConnectionContext {
            contact_timeout: self.contact_timeout,
            keepalive_interval: self.keepalive_interval,
            segment_mru: self.segment_mru,
            transfer_mru: self.transfer_mru,
            node_ids: self.node_ids.clone(),
            sink: self.sink.clone(),
            registry: self.registry.clone(),
            tls: self.tls.clone(),
            session_cancel_token: self.session_cancel_token.clone(),
            task_cancel_token: self.tasks.cancel_token().clone(),
        }
    }

    // Starting consumes the entity's bound listener sockets, so it takes
    // effect exactly once.
    fn start_listeners(&self) {
        let listeners = std::mem::take(
            &mut *self
                .listeners
                .lock()
                .trace_expect("listener mutex poisoned"),
        );
        for listener in listeners {
            let acceptor = Listener {
                connection_rate_limit: self.connection_rate_limit,
                ctx: self.connection_context(),
            };
            self.tasks
                .spawn(acceptor.listen(self.tasks.clone(), listener));
        }
    }
}

// Every input that shapes the entity's behaviour; the runtime state
// (sessions, the BPA sink) has no useful text form. TLS shows only
// whether it is required: the material holds key bytes.
impl Debug for Tcpclv4 {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tcpclv4")
            .field("listen_addresses", &self.listen_addresses)
            .field("segment_mru", &self.segment_mru)
            .field("transfer_mru", &self.transfer_mru)
            .field("connection_rate_limit", &self.connection_rate_limit)
            .field("contact_timeout", &self.contact_timeout)
            .field("keepalive_interval", &self.keepalive_interval)
            .field(
                "tls_required",
                &self.tls.as_ref().map(|tls| tls.is_required()),
            )
            .field("node_ids", &self.node_ids)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Cla for Tcpclv4 {
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn register(&self) -> cla::Result<()> {
        self.start_listeners();
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn unregister(&self) -> cla::Result<()> {
        // Cancel sessions first so they exit promptly when channels close
        self.session_cancel_token.cancel();

        // Shutdown all pooled connections (drops tx senders)
        self.registry.shutdown();

        // Wait for all session tasks to complete
        self.tasks.shutdown().await;

        Ok(())
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
        cla_addr: &Address,
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<ForwardOutcome> {
        let Address::Tcp(remote_addr) = cla_addr else {
            return Ok(ForwardOutcome::NoNeighbour);
        };

        let ctx = self.connection_context();
        let bundle = buffer_stream(stream, total_len).await?;

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
            if let Err(e) = ctx.sink.report_transfer(&bundle_id, outcome).await {
                debug!("Failed to report transfer outcome: {e}");
            }
        });

        Ok(ForwardOutcome::Accepted)
    }
}

// Peers can close at random times, so a transfer is retried this many
// times before falling back to a last try on a busy session.
const DIAL_ATTEMPTS: usize = 5;

// Transmit a bundle to `remote_addr` over a pooled session, dialing new
// connections as the pool allows, and return the terminal outcome of the
// transfer: `Completed` only once the peer has fully acknowledged it.
async fn transmit(
    tasks: Arc<hardy_async::TaskPool>,
    ctx: &ConnectionContext,
    remote_addr: SocketAddr,
    mut bundle: Bytes,
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

        // One dial at a time per peer: concurrent forwards coalesce here
        // rather than racing parallel dials, and the pool is re-checked
        // under the lock, since the previous holder's session may already
        // be registered.
        let dial_lock = ctx.registry.dial_lock(remote_addr);
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
        let conn = Connector {
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
    use core::num::NonZeroU64;
    use std::sync::Arc;

    use hardy_bpa::{
        async_trait,
        cla::{self, Acceptance, Address, Context, Factory as _, Sink, Terms, TransferOutcome},
        node_ids::NodeIds,
        stream::Receiver,
    };
    use hardy_bpv7::{bundle::Id, eid::NodeId};

    use super::Tcpclv4;
    use crate::{KeepaliveInterval, error::Error, tls::Tls};

    struct MockSink;

    #[async_trait]
    impl Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&Address>,
            _stream: &mut dyn Receiver<cla::Segment>,
        ) -> cla::Result<Acceptance> {
            Ok(Acceptance::Accepted)
        }

        async fn add_peer(&self, _cla_addr: Address, _node_ids: &[NodeId]) -> cla::Result<bool> {
            Ok(true)
        }

        async fn remove_peer(&self, _cla_addr: &Address) -> cla::Result<bool> {
            Ok(true)
        }

        async fn report_transfer(
            &self,
            _bundle_id: &Id,
            _outcome: TransferOutcome,
        ) -> cla::Result<()> {
            Ok(())
        }
    }

    // Builds the entity under the receive limit a BPA settled on, as the
    // CLA table does.
    fn build(
        factory: impl cla::Factory<Component = Tcpclv4>,
        max_bundle_size: Option<NonZeroU64>,
    ) -> Arc<Tcpclv4> {
        let terms = Terms {
            max_bundle_size,
            ..Default::default()
        };
        factory
            .build(Context::new(
                Arc::new(MockSink),
                Arc::new(NodeIds::default()),
                terms,
            ))
            .expect("a TCPCLv4 entity builds under any terms")
    }

    // The transfer MRU sessions advertise is the configured value folded
    // with the settled receive limit: a smaller limit clamps it, a larger
    // or absent one leaves it alone.
    #[test]
    fn build_folds_transfer_mru_with_the_settled_limit() {
        let configured = NonZeroU64::new(1024 * 1024).unwrap();
        let smaller = NonZeroU64::new(configured.get() / 2).unwrap();
        let larger = NonZeroU64::new(configured.get() * 2).unwrap();

        let factory = Tcpclv4::builder().transfer_mru(configured).build().unwrap();
        assert_eq!(build(factory, Some(smaller)).transfer_mru, smaller);

        let factory = Tcpclv4::builder().transfer_mru(configured).build().unwrap();
        assert_eq!(build(factory, Some(larger)).transfer_mru, configured);

        let factory = Tcpclv4::builder().transfer_mru(configured).build().unwrap();
        assert_eq!(build(factory, None).transfer_mru, configured);
    }

    // The segment MRU sessions advertise is clamped to the folded transfer
    // MRU: a SESS_INIT never invites a single segment larger than any
    // completable transfer.
    #[test]
    fn build_clamps_segment_mru_to_the_folded_transfer_mru() {
        let segment = NonZeroU64::new(16 * 1024).unwrap();
        let transfer = NonZeroU64::new(1024 * 1024).unwrap();
        let below_segment = NonZeroU64::new(segment.get() / 2).unwrap();

        // A settled limit below the segment MRU drags the segment MRU
        // down along with the transfer MRU.
        let factory = Tcpclv4::builder()
            .segment_mru(segment)
            .transfer_mru(transfer)
            .build()
            .unwrap();
        let cla = build(factory, Some(below_segment));
        assert_eq!(cla.transfer_mru, below_segment);
        assert_eq!(cla.segment_mru, below_segment);

        // A transfer MRU above the segment MRU leaves it alone.
        let factory = Tcpclv4::builder()
            .segment_mru(segment)
            .transfer_mru(transfer)
            .build()
            .unwrap();
        assert_eq!(build(factory, None).segment_mru, segment);
    }

    // The entity declares the TCP address type and its configured
    // transfer MRU, which is all the BPA needs to settle terms for it.
    #[test]
    fn the_request_declares_tcp_and_the_configured_transfer_mru() {
        let configured = NonZeroU64::new(1024 * 1024).unwrap();
        let request = Tcpclv4::builder()
            .transfer_mru(configured)
            .build()
            .unwrap()
            .request();
        assert_eq!(request.address_type, Some(cla::AddressType::Tcp));
        assert_eq!(request.max_bundle_size, Some(configured));
    }

    // RFC 9174 Section 2.1: an entity may support zero or more passive
    // listening elements, each bound during build().
    #[test]
    fn listen_adds_bound_passive_listening_elements() {
        let factory = Tcpclv4::builder()
            .build()
            .expect("a dial-only plaintext build cannot fail");
        let cla = build(factory, None);
        assert!(cla.listen_addresses.is_empty());
        assert!(cla.tls.is_none());

        let factory = Tcpclv4::builder()
            .listen("[::1]:0".parse().unwrap())
            .listen("[::1]:0".parse().unwrap())
            .build()
            .unwrap();
        let cla = build(factory, None);
        assert_eq!(cla.listen_addresses.len(), 2);
        for address in &cla.listen_addresses {
            assert_ne!(address.port(), 0);
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
        assert!(matches!(err, Error::BindListener { .. }));
    }

    // Requiring TLS without an identity leaves listeners nothing they
    // could serve, rejected before any socket is bound.
    #[test]
    fn required_tls_listeners_need_an_identity() {
        let Err(err) = Tcpclv4::builder()
            .listen("[::1]:0".parse().unwrap())
            .tls(
                Tls::builder()
                    .dangerous()
                    .insecure_skip_verify()
                    .required(true),
            )
            .build()
        else {
            panic!("listeners under identity-less required TLS must fail");
        };
        assert!(matches!(err, Error::RequiredTlsWithoutIdentity));
    }

    #[test]
    fn no_keepalive_disables_keepalives() {
        let factory = Tcpclv4::builder().no_keepalive().build().unwrap();
        assert!(build(factory, None).keepalive_interval.is_disabled());
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
        let factory = Tcpclv4::builder()
            .tls(
                Tls::builder()
                    .dangerous()
                    .insecure_skip_verify()
                    .required(true),
            )
            .build()
            .unwrap();
        let cla = build(factory, None);
        let tls = cla.tls.as_ref().expect("TLS material must be present");
        assert!(tls.is_required());
    }
}
