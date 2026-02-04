mod cla;
mod codec;
mod connect;
mod connection;
mod listen;
mod session;
mod tls;
mod transport;

pub mod config;

use hardy_bpv7::eid::NodeId;
use std::sync::Arc;
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "tracing")]
use tracing::instrument;

struct ClaInner {
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    node_ids: Arc<[NodeId]>,
    registry: Arc<connection::ConnectionRegistry>,
    tls_config: Option<Arc<tls::TlsConfig>>,
}

pub struct Cla {
    _name: String,
    config: config::Config,
    inner: std::sync::OnceLock<ClaInner>,
    tasks: Arc<hardy_async::TaskPool>,
}

impl Cla {
    pub fn new(name: String, config: config::Config) -> Self {
        if config.session_defaults.must_use_tls && config.tls.is_none() {
            error!("{name}: TLS is required, but no TLS configuration has been provided");
        }

        if config.session_defaults.contact_timeout > 60 {
            warn!("{name}: RFC9174 specifies contact timeout SHOULD be a maximum of 60 seconds");
        }

        match config.session_defaults.keepalive_interval {
            None | Some(0) => info!("{name}: Session keepalive disabled"),
            Some(x) if x < 15 => {
                warn!("{name}: RFC9174 specifies contact timeout SHOULD be a minimum of 15 seconds")
            }
            Some(x) if x > 600 => {
                warn!("{name}: RFC9174 specifies keepalive SHOULD be a maximum of 600 seconds")
            }
            _ => {}
        }

        Self {
            config,
            _name: name,
            inner: std::sync::OnceLock::new(),
            tasks: Arc::new(hardy_async::TaskPool::new()),
        }
    }

    // Unregisters the CLA instance from the BPA.
    pub async fn unregister(&self) -> bool {
        let Some(inner) = self.inner.get() else {
            return false;
        };
        inner.sink.unregister().await;
        true
    }

    pub async fn connect(&self, remote_addr: &std::net::SocketAddr) -> hardy_bpa::cla::Result<()> {
        let Some(inner) = self.inner.get() else {
            error!("connect called before on_register!");
            return Err(hardy_bpa::cla::Error::Disconnected);
        };

        for _ in 0..5 {
            // Do a new active connect
            let conn = connect::Connector {
                tasks: self.tasks.clone(),
                contact_timeout: self.config.session_defaults.contact_timeout,
                must_use_tls: self.config.session_defaults.must_use_tls,
                keepalive_interval: self.config.session_defaults.keepalive_interval,
                segment_mru: self.config.segment_mru,
                transfer_mru: self.config.transfer_mru,
                node_ids: inner.node_ids.clone(),
                sink: inner.sink.clone(),
                registry: inner.registry.clone(),
                tls_config: inner.tls_config.clone(),
            };
            match conn.connect(remote_addr).await {
                Ok(()) => return Ok(()),
                Err(transport::Error::Timeout) => {}
                Err(e) => {
                    // No point retrying
                    return Err(hardy_bpa::cla::Error::Internal(e.into()));
                }
            }
        }
        Err(hardy_bpa::cla::Error::Internal(
            transport::Error::Timeout.into(),
        ))
    }
}
