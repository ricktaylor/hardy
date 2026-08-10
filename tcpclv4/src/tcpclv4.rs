//! [`Tcpclv4`]: the TCPCL entity — the CLA that registers with a BPA.

use core::num::NonZeroU16;
use std::net::TcpListener;
use std::sync::Mutex;

use hardy_bpa::async_trait;

use super::*;

/// Seconds to wait for a peer's contact header, bounded so that a value
/// outside RFC 9174 Section 4.2's recommendation (at most 60 seconds, and
/// never the instant timeout of zero) is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactTimeout(u16);

impl ContactTimeout {
    /// The RFC 9174 Section 4.2 recommended maximum: 60 seconds.
    pub const MAX: ContactTimeout = ContactTimeout(60);

    /// Creates a contact timeout; `None` when `seconds` is zero or above
    /// [`MAX`](Self::MAX).
    pub const fn new(seconds: u16) -> Option<Self> {
        if seconds == 0 || seconds > Self::MAX.0 {
            None
        } else {
            Some(Self(seconds))
        }
    }

    /// The timeout in whole seconds.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Keepalive interval proposed during session negotiation, in seconds; an
/// interval that exists is never zero, so "disabled" is `None` at rest.
///
/// `Option<KeepaliveInterval>` is still one `u16` wide: `None` occupies
/// the zero niche, which is exactly the wire encoding RFC 9174 Section
/// 4.7 gives to "KEEPALIVEs are disabled".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepaliveInterval(NonZeroU16);

impl KeepaliveInterval {
    /// Creates a keepalive interval; `None` when `seconds` is zero (the
    /// wire encoding for disabled keepalives).
    pub const fn new(seconds: u16) -> Option<Self> {
        match NonZeroU16::new(seconds) {
            Some(seconds) => Some(Self(seconds)),
            None => None,
        }
    }

    /// The interval in whole seconds.
    pub const fn get(self) -> u16 {
        self.0.get()
    }

    /// Negotiates the session keepalive against the peer's SESS_INIT
    /// proposal: the minimum of the two (RFC 9174 Section 4.7), where
    /// disabled (`None` locally, zero from the peer) wins.
    pub fn negotiate(local: Option<Self>, peer_keepalive: u16) -> u16 {
        local.map_or(0, |interval| interval.get().min(peer_keepalive))
    }
}

/// Registration-time state from BPA.
struct Inner {
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    node_ids: Arc<[NodeId]>,
}

/// TCPCLv4 Convergence Layer Adapter (RFC 9174).
///
/// Manages TCP connections to peer nodes, including listener and connector
/// roles, optional TLS, keepalive, and transfer segmentation. Implements
/// the BPA CLA trait so it can be registered with a BPA instance.
pub struct Tcpclv4 {
    // Builder inputs
    contact_timeout: ContactTimeout,
    keepalive_interval: Option<KeepaliveInterval>,
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
        keepalive_interval: Option<KeepaliveInterval>,
        listeners: Vec<std::net::TcpListener>,
        connection_rate_limit: NonZeroU32,
        segment_mru: NonZeroU64,
        transfer_mru: NonZeroU64,
        max_idle_connections: usize,
        max_outstanding_transfers: core::num::NonZeroUsize,
        tls: Option<Arc<tls::Tls>>,
    ) -> Self {
        Self {
            contact_timeout,
            keepalive_interval,
            listeners: std::sync::Mutex::new(listeners),
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

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        if let Some(inner) = self.inner.get() {
            inner.sink.unregister().await;
        }
    }

    /// Initiates a TCP connection to a remote peer (RFC 9174 Section 3).
    ///
    /// Retries up to 5 times on timeout before returning an error.
    pub async fn connect(&self, remote_addr: &SocketAddr) -> hardy_bpa::cla::Result<()> {
        let ctx = self.connection_context().ok_or_else(|| {
            error!("connect called before on_register!");
            hardy_bpa::cla::Error::Disconnected
        })?;

        for _ in 0..5 {
            let conn = connection::connect::Connector {
                tasks: self.tasks.clone(),
                ctx: ctx.clone(),
            };
            match conn.connect(remote_addr).await {
                Ok(()) => return Ok(()),
                Err(transport::Error::Timeout) => {}
                Err(e) => {
                    return Err(hardy_bpa::cla::Error::Internal(e.into()));
                }
            }
        }
        Err(hardy_bpa::cla::Error::Internal(
            transport::Error::Timeout.into(),
        ))
    }

    /// Creates a ConnectionContext for use in connect/forward operations.
    fn connection_context(&self) -> Option<connection::context::ConnectionContext> {
        let inner = self.inner.get()?;

        Some(connection::context::ConnectionContext {
            contact_timeout: self.contact_timeout,
            keepalive_interval: self.keepalive_interval,
            segment_mru: self.segment_mru,
            transfer_mru: self.transfer_mru,
            node_ids: inner.node_ids.clone(),
            sink: inner.sink.clone(),
            registry: self.registry.clone(),
            tls: self.tls.clone(),
            session_cancel_token: self.session_cancel_token.clone(),
            task_cancel_token: self.tasks.cancel_token().clone(),
        })
    }

    fn start_listeners(&self) {
        // The sockets were bound by the builder; take them and start
        // accepting. A second registration finds the vector empty
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
impl hardy_bpa::cla::Cla for Tcpclv4 {
    fn address_type(&self) -> Option<hardy_bpa::cla::ClaAddressType> {
        Some(hardy_bpa::cla::ClaAddressType::Tcp)
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, sink)))]
    async fn on_register(&self, sink: Box<dyn hardy_bpa::cla::Sink>, node_ids: &[NodeId]) {
        // Store sink and node_ids in single atomic operation
        self.inner.call_once(|| Inner {
            sink: sink.into(),
            node_ids: node_ids.into(),
        });

        // Start listeners now that we have a sink
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

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn forward(
        &self,
        _queue: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        bundle_id: &hardy_bpv7::bundle::Id,
        bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        let ctx = self.connection_context().ok_or_else(|| {
            error!("forward called before on_register!");
            hardy_bpa::cla::Error::Disconnected
        })?;

        let hardy_bpa::cla::ClaAddress::Tcp(remote_addr) = cla_addr else {
            return Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour);
        };

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

        Ok(hardy_bpa::cla::ForwardBundleResult::Accepted)
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
    ctx: &connection::context::ConnectionContext,
    remote_addr: SocketAddr,
    mut bundle: hardy_bpa::Bytes,
) -> hardy_bpa::cla::TransferOutcome {
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
        // under the lock — the previous holder's session may already be
        // registered
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
        let conn = connection::connect::Connector {
            tasks: tasks.clone(),
            ctx: ctx.clone(),
        };
        match conn.connect(&remote_addr).await {
            Ok(()) => {}
            Err(transport::Error::Timeout) if !ctx.registry.has_sessions(&remote_addr) => {
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
                    .unwrap_or(hardy_bpa::cla::TransferOutcome::Failed);
            }
        }
    }

    // Repeated dial timeouts: last try on a busy session before
    // giving up on the transfer
    ctx.registry
        .forward(&remote_addr, bundle, connection::OnBusy::Queue)
        .await
        .unwrap_or(hardy_bpa::cla::TransferOutcome::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(err, builder::Error::Listen { .. }));
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
        assert!(matches!(err, builder::Error::ListenersWithoutIdentity));
    }

    #[test]
    fn no_keepalive_disables_keepalives() {
        let cla = Tcpclv4::builder().no_keepalive().build().unwrap();
        assert!(cla.keepalive_interval.is_none());
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
