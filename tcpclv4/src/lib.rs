mod cla;
mod codec;
mod connect;
mod connection;
mod listen;
mod session;
mod transport;
mod tls;

pub mod config;

use hardy_bpv7::eid::Eid;
use std::sync::Arc;
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "tracing")]
use tracing::{Instrument, instrument};

struct ClaInner {
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    node_ids: Arc<[Eid]>,
    registry: Arc<connection::ConnectionRegistry>,
    tls_config: Option<Arc<tls::TlsConfig>>,
}

pub struct Cla {
    _name: String,
    config: config::Config,
    inner: std::sync::OnceLock<ClaInner>,
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
}

impl Cla {
    pub fn new(name: String, config: config::Config) -> Self {
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
            cancel_token: tokio_util::sync::CancellationToken::new(),
            task_tracker: tokio_util::task::TaskTracker::new(),
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

    pub async fn add_peer(&self, remote_addr: std::net::SocketAddr, eid: Eid) -> bool {
        self.inner
            .get()
            .trace_expect("CLA not registered")
            .registry
            .add_peer(remote_addr, eid)
            .await
    }

    pub async fn remove_peer(&self, remote_addr: &std::net::SocketAddr) -> bool {
        self.inner
            .get()
            .trace_expect("CLA not registered")
            .registry
            .remove_peer(remote_addr)
            .await
    }
}
