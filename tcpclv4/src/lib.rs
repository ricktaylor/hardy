mod cla;
mod codec;
mod connect;
mod connection;
mod context;
mod listen;
mod session;
mod tls;
mod transport;

pub mod config;

use hardy_async::sync::spin::Once;
use hardy_bpv7::eid::NodeId;
use std::net::SocketAddr;
use std::sync::Arc;
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "tracing")]
use tracing::instrument;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("TLS is required but no TLS configuration has been provided")]
    TlsRequired,

    #[error("TLS configuration error: {0}")]
    Tls(#[from] tls::TlsError),

    #[error("Registration failed: {0}")]
    Registration(#[from] hardy_bpa::cla::Error),
}

/// Registration-time state from BPA.
struct Inner {
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    node_ids: Arc<[NodeId]>,
}

pub struct Cla {
    // Config values
    session_config: config::SessionConfig,
    address: Option<SocketAddr>,
    connection_rate_limit: u32,
    segment_mru: u64,
    transfer_mru: u64,

    // Computed at construction
    tls_config: Option<Arc<tls::TlsConfig>>,
    registry: Arc<connection::ConnectionRegistry>,
    session_cancel_token: tokio_util::sync::CancellationToken,

    // Late-init from registration (single atomic)
    inner: Once<Inner>,

    // Task management
    tasks: Arc<hardy_async::TaskPool>,
}

impl Cla {
    /// Creates a new TCPCLv4 CLA instance.
    ///
    /// # Arguments
    ///
    /// * `config` - The configuration for this CLA.
    ///
    /// # Errors
    ///
    /// Returns an error if TLS is required but not configured, or if TLS
    /// configuration files cannot be loaded.
    pub fn new(config: &config::Config) -> Result<Self, Error> {
        // Validate TLS requirement
        if config.session_defaults.must_use_tls && config.tls.is_none() {
            return Err(Error::TlsRequired);
        }

        // Warn about RFC compliance
        if config.session_defaults.contact_timeout > 60 {
            warn!("RFC9174 specifies contact timeout SHOULD be a maximum of 60 seconds");
        }

        match config.session_defaults.keepalive_interval {
            None | Some(0) => debug!("Session keepalive disabled"),
            Some(x) if x < 30 => {
                warn!(
                    "RFC9174 Section 5.1.1 specifies keepalive SHOULD be a minimum of 30 seconds for shared networks"
                )
            }
            Some(x) if x > 600 => {
                warn!("RFC9174 specifies keepalive SHOULD be a maximum of 600 seconds")
            }
            _ => {}
        }

        // Load TLS configuration eagerly
        let tls_config = if let Some(tls_cfg) = &config.tls {
            let cfg = tls::TlsConfig::new(tls_cfg)?;
            info!("TLS configuration loaded successfully");
            Some(Arc::new(cfg))
        } else {
            warn!(
                "No TLS configuration provided - connections will be unencrypted and TLS-requiring peers will refuse connection"
            );
            None
        };

        Ok(Self {
            // Config values
            session_config: config.session_defaults.clone(),
            address: config.address,
            connection_rate_limit: config.connection_rate_limit,
            segment_mru: config.segment_mru,
            transfer_mru: config.transfer_mru,

            // Computed state
            tls_config,
            registry: Arc::new(connection::ConnectionRegistry::new(
                config.max_idle_connections,
            )),
            session_cancel_token: tokio_util::sync::CancellationToken::new(),

            // Late-init
            inner: Once::new(),

            // Tasks
            tasks: Arc::new(hardy_async::TaskPool::new()),
        })
    }

    /// Registers this CLA with the BPA.
    ///
    /// # Arguments
    ///
    /// * `bpa` - The BPA registration interface (local Bpa or remote via gRPC).
    /// * `name` - The name to register this CLA under.
    /// * `policy` - Optional egress policy for this CLA.
    pub async fn register(
        self: &Arc<Self>,
        bpa: &dyn hardy_bpa::bpa::BpaRegistration,
        name: String,
        policy: Option<Arc<dyn hardy_bpa::policy::EgressPolicy>>,
    ) -> Result<(), Error> {
        bpa.register_cla(
            name,
            Some(hardy_bpa::cla::ClaAddressType::Tcp),
            self.clone(),
            policy,
        )
        .await?;
        Ok(())
    }

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        if let Some(inner) = self.inner.get() {
            inner.sink.unregister().await;
        }
    }

    /// Creates a ConnectionContext for use in connect/forward operations.
    fn connection_context(&self) -> Option<context::ConnectionContext> {
        let inner = self.inner.get()?;

        Some(context::ConnectionContext {
            session: self.session_config.clone(),
            segment_mru: self.segment_mru,
            transfer_mru: self.transfer_mru,
            node_ids: inner.node_ids.clone(),
            sink: inner.sink.clone(),
            registry: self.registry.clone(),
            tls_config: self.tls_config.clone(),
            session_cancel_token: self.session_cancel_token.clone(),
            task_cancel_token: self.tasks.cancel_token().clone(),
        })
    }

    pub async fn connect(&self, remote_addr: &SocketAddr) -> hardy_bpa::cla::Result<()> {
        let ctx = self.connection_context().ok_or_else(|| {
            error!("connect called before on_register!");
            hardy_bpa::cla::Error::Disconnected
        })?;

        for _ in 0..5 {
            let conn = connect::Connector {
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
}
