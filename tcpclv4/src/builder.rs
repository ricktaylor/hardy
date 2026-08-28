//! [`Tcpclv4Builder`]: the fluent constructor for a [`Tcpclv4`].

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::Arc;

use tracing::{debug, warn};

use crate::{
    ContactTimeout, KeepaliveInterval, Tcpclv4,
    error::{Error, Result},
    otel_metrics,
    tls::Tls,
};

/// Builder for a [`Tcpclv4`]. Obtain one from [`Tcpclv4::builder`].
///
/// Every setting starts at the default documented on its method, so only deviations need to be chained before [`build()`](Self::build).
#[must_use = "a Tcpclv4Builder does nothing unless `build()` is called"]
pub struct Tcpclv4Builder {
    listeners: Vec<SocketAddr>,
    segment_mru: NonZeroU64,
    transfer_mru: NonZeroU64,
    max_idle_connections: usize,
    max_outstanding_transfers: NonZeroUsize,
    connection_rate_limit: NonZeroU32,
    contact_timeout: ContactTimeout,
    keepalive_interval: KeepaliveInterval,
    tls: Option<Tls>,
}

impl Tcpclv4Builder {
    // The private defaults applied by `new()`; each value is documented
    // on its setter, the single place it is part of the API.
    // `REGISTERED_LISTEN_ADDRESS` is the IANA-registered TCPCL address,
    // on all interfaces: port 4556 (RFC 9174 Section 4.1).
    const REGISTERED_LISTEN_ADDRESS: SocketAddr =
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 4556);
    const DEFAULT_SEGMENT_MRU: NonZeroU64 = NonZeroU64::new(16384).unwrap();
    const DEFAULT_TRANSFER_MRU: NonZeroU64 = NonZeroU64::new(0x4000_0000).unwrap();
    const DEFAULT_MAX_IDLE_CONNECTIONS: usize = 6;
    const DEFAULT_MAX_OUTSTANDING_TRANSFERS: NonZeroUsize = NonZeroUsize::new(16).unwrap();
    const DEFAULT_CONNECTION_RATE_LIMIT: NonZeroU32 = NonZeroU32::new(64).unwrap();
    const DEFAULT_CONTACT_TIMEOUT: ContactTimeout = ContactTimeout::new(15).unwrap();
    const DEFAULT_KEEPALIVE_INTERVAL: KeepaliveInterval = KeepaliveInterval::new(60);

    pub(crate) fn new() -> Self {
        Self {
            listeners: Vec::new(),
            segment_mru: Self::DEFAULT_SEGMENT_MRU,
            transfer_mru: Self::DEFAULT_TRANSFER_MRU,
            max_idle_connections: Self::DEFAULT_MAX_IDLE_CONNECTIONS,
            max_outstanding_transfers: Self::DEFAULT_MAX_OUTSTANDING_TRANSFERS,
            connection_rate_limit: Self::DEFAULT_CONNECTION_RATE_LIMIT,
            contact_timeout: Self::DEFAULT_CONTACT_TIMEOUT,
            keepalive_interval: Self::DEFAULT_KEEPALIVE_INTERVAL,
            tls: None,
        }
    }

    /// Adds a passive listening element accepting incoming sessions on `address` (RFC 9174 Section 2.1).
    ///
    /// An entity may support zero or more listening elements: chain this once per listener. Without any, the entity only dials out. The socket is bound during [`build()`](Self::build); [`listen_default`](Self::listen_default) is the IANA-registered choice.
    pub fn listen(mut self, address: SocketAddr) -> Self {
        self.listeners.push(address);
        self
    }

    /// Adds a passive listening element on the IANA-registered TCPCL address, `[::]:4556` on all interfaces (RFC 9174 Section 4.1).
    pub fn listen_default(self) -> Self {
        self.listen(Self::REGISTERED_LISTEN_ADDRESS)
    }

    /// Largest acceptable single-segment payload advertised in SESS_INIT, in bytes (RFC 9174 Section 4.6).
    ///
    /// Default: 16384 bytes.
    pub fn segment_mru(mut self, mru: NonZeroU64) -> Self {
        self.segment_mru = mru;
        self
    }

    /// Largest acceptable total transfer (bundle) size advertised in SESS_INIT, in bytes (RFC 9174 Section 4.6).
    ///
    /// Default: 1 GiB.
    pub fn transfer_mru(mut self, mru: NonZeroU64) -> Self {
        self.transfer_mru = mru;
        self
    }

    /// Idle connections retained per remote address for reuse by later transfers; zero disables pooling.
    ///
    /// Default: 6.
    pub fn max_idle_connections(mut self, limit: usize) -> Self {
        self.max_idle_connections = limit;
        self
    }

    /// Transfers accepted from the BPA but not yet resolved with an outcome, per peer.
    ///
    /// Bounds the bundles held in memory by in-flight and queued transfers to each peer; at the limit, further forwards to that peer are held unanswered, which is the flow control back to the BPA. Default: 16.
    pub fn max_outstanding_transfers(mut self, limit: NonZeroUsize) -> Self {
        self.max_outstanding_transfers = limit;
        self
    }

    /// Inbound connections accepted per second; the listeners delay accepts beyond this rate.
    ///
    /// Default: 64.
    pub fn connection_rate_limit(mut self, per_second: NonZeroU32) -> Self {
        self.connection_rate_limit = per_second;
        self
    }

    /// Time to wait for the peer's contact header before giving up on a connection.
    ///
    /// [`ContactTimeout`] bounds the wait to RFC 9174 Section 4.2's recommended maximum. Default: 15 seconds.
    pub fn contact_timeout(mut self, timeout: ContactTimeout) -> Self {
        self.contact_timeout = timeout;
        self
    }

    /// Keepalive interval proposed during session negotiation (RFC 9174 Section 5.1.1).
    ///
    /// The negotiated value is the minimum of both peers' proposals. RFC 9174 recommends 30 to 600 seconds on shared networks, and values outside that range are accepted with a warning; zero ([`KeepaliveInterval::DISABLED`]) disables keepalives. Default: 60 seconds.
    pub fn keepalive_interval(mut self, interval: KeepaliveInterval) -> Self {
        if interval.is_disabled() {
            debug!("Session keepalive disabled");
        } else if interval.get() < 30 {
            warn!(
                "RFC9174 Section 5.1.1 specifies keepalive SHOULD be a minimum of 30 seconds for shared networks"
            );
        } else if interval.get() > 600 {
            warn!("RFC9174 specifies keepalive SHOULD be a maximum of 600 seconds");
        }
        self.keepalive_interval = interval;
        self
    }

    /// Disables session keepalives: the SESS_INIT proposal is the wire zero.
    pub fn no_keepalive(self) -> Self {
        self.keepalive_interval(KeepaliveInterval::DISABLED)
    }

    /// Enables TLS with this material (RFC 9174 Section 4.4): sessions negotiate TLS when the peer also advertises it, and plaintext peers are still accepted unless the material was built with [`TlsBuilder::required`](crate::tls::TlsBuilder::required).
    ///
    /// Build the material with [`Tls::builder`]. Replaces any earlier TLS choice. Listeners offer TLS only when an identity is configured; combining listeners with an identity-less required-TLS policy is rejected by [`build()`](Self::build).
    pub fn tls(mut self, tls: Tls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Binds the listeners and assembles the [`Tcpclv4`], ready to register with a BPA.
    ///
    /// # Errors
    ///
    /// Returns an [`enum@Error`] when a listener socket cannot be bound, or when listeners are combined with an identity-less required-TLS policy; a dial-only plaintext build cannot fail.
    pub fn build(self) -> Result<Tcpclv4> {
        // Register metric descriptions with the global recorder
        otel_metrics::init();

        let tls = self.tls.map(Arc::new);
        if tls.is_none() {
            warn!(
                "No TLS configuration provided - connections will be unencrypted and TLS-requiring peers will refuse connection"
            );
        }

        // Listeners under a required-TLS policy need an identity to serve
        if let Some(tls) = &tls
            && tls.required_without_identity()
            && !self.listeners.is_empty()
        {
            return Err(Error::RequiredTlsWithoutIdentity);
        }

        // Bind the passive listening elements eagerly, so socket failures
        // (port in use, missing privileges) surface here rather than in a
        // background task; accepting starts at BPA registration
        let mut listeners = Vec::with_capacity(self.listeners.len());
        for address in self.listeners {
            let listener = TcpListener::bind(address)
                .and_then(|listener| {
                    // The runtime's reactor requires non-blocking sockets
                    listener.set_nonblocking(true).map(|()| listener)
                })
                .map_err(|source| Error::BindListener { address, source })?;
            listeners.push(listener);
        }

        Ok(Tcpclv4::new(
            self.contact_timeout,
            self.keepalive_interval,
            listeners,
            self.connection_rate_limit,
            self.segment_mru,
            self.transfer_mru,
            self.max_idle_connections,
            self.max_outstanding_transfers,
            tls,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every default matches its defining const, no listener exists
    // until one is added, and a plaintext build cannot fail.
    #[test]
    fn defaults_are_applied() {
        let builder = Tcpclv4::builder();
        assert!(builder.listeners.is_empty());
        assert_eq!(builder.segment_mru, Tcpclv4Builder::DEFAULT_SEGMENT_MRU);
        assert_eq!(builder.transfer_mru, Tcpclv4Builder::DEFAULT_TRANSFER_MRU);
        assert_eq!(
            builder.max_idle_connections,
            Tcpclv4Builder::DEFAULT_MAX_IDLE_CONNECTIONS
        );
        assert_eq!(
            builder.connection_rate_limit,
            Tcpclv4Builder::DEFAULT_CONNECTION_RATE_LIMIT
        );
        assert_eq!(
            builder.contact_timeout,
            Tcpclv4Builder::DEFAULT_CONTACT_TIMEOUT
        );
        assert_eq!(
            builder.keepalive_interval,
            Tcpclv4Builder::DEFAULT_KEEPALIVE_INTERVAL
        );

        builder
            .build()
            .expect("a dial-only plaintext build cannot fail");
    }

    // listen_default() is sugar for the IANA-registered address, kept
    // private so the intention has exactly one public spelling.
    #[test]
    fn listen_default_is_the_registered_address() {
        let builder = Tcpclv4Builder::new().listen_default();
        assert_eq!(
            builder.listeners,
            vec![Tcpclv4Builder::REGISTERED_LISTEN_ADDRESS]
        );
    }

    // RFC 9174 Section 2.1: an entity may support zero or more passive
    // listening elements; listen() accumulates one address per call.
    #[test]
    fn listen_accumulates_listening_elements() {
        let one = crate::tests::loopback();
        let two = crate::tests::loopback();
        let builder = Tcpclv4Builder::new().listen(one).listen(two);
        assert_eq!(builder.listeners, vec![one, two]);
    }

    // A socket that cannot be bound fails the build instead of a
    // background accept task.
    #[test]
    fn bind_failures_surface_at_build() {
        let occupied = TcpListener::bind(crate::tests::loopback()).unwrap();
        let address = occupied.local_addr().unwrap();
        let Err(err) = Tcpclv4Builder::new().listen(address).build() else {
            panic!("binding an occupied port must fail");
        };
        assert!(matches!(err, Error::BindListener { .. }));
    }

    // Requiring TLS without an identity leaves listeners nothing they
    // could serve, rejected before any socket is bound.
    #[test]
    fn required_tls_listeners_need_an_identity() {
        let Err(err) = Tcpclv4Builder::new()
            .listen(crate::tests::loopback())
            .tls(
                Tls::builder()
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
        assert!(matches!(err, Error::RequiredTlsWithoutIdentity));
    }

    #[test]
    fn no_keepalive_disables_keepalives() {
        let builder = Tcpclv4Builder::new().no_keepalive();
        assert!(builder.keepalive_interval.is_disabled());
    }

    // The insecure stage needs no files, so the Required policy is
    // observable without touching disk.
    #[test]
    fn required_material_lands_as_the_required_policy() {
        let builder = Tcpclv4Builder::new().tls(
            Tls::builder()
                .dangerous()
                .insecure_skip_verify()
                .required(true)
                .build()
                .unwrap(),
        );
        let tls = builder.tls.as_ref().expect("TLS material must be present");
        assert!(tls.is_required());
    }
}
