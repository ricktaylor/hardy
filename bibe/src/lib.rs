#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

mod cla;
mod config;
mod service;

pub use config::{Config, Tunnel};

// Common imports for submodules (accessed via `use super::*;`)
use alloc::{borrow::Cow, boxed::Box, sync::Arc, vec::Vec};
use hardy_async::sync::spin::Once;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::{Bytes, async_trait};
use hardy_bpv7::{
    bpsec,
    bundle::ParsedBundle,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, NodeId, Service},
};
use tracing::{debug, error, warn};

/// BIBE tunnel endpoint manager.
///
/// Provides Bundle-in-Bundle Encapsulation for tunneling bundles
/// through intermediate networks. Uses a hybrid architecture:
/// - CLA for encapsulation (via `forward()`)
/// - Service for decapsulation (via `on_receive()`)
pub struct Bibe {
    // Config values needed for registration
    decap_service_id: Option<Service>,
    tunnels: Vec<Tunnel>,

    // Internal components
    cla: Arc<cla::BibeCla>,
    decap_service: Arc<service::DecapService>,
}

impl Bibe {
    /// Create a new BIBE instance from configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - The configuration for this BIBE instance.
    pub fn new(config: &Config) -> Self {
        let cla = Arc::new(cla::BibeCla::new(config.tunnel_source.clone()));
        let decap_service = Arc::new(service::DecapService::new(cla.clone()));

        Self {
            decap_service_id: config.decap_service_id.clone(),
            tunnels: config.tunnels.clone(),
            cla,
            decap_service,
        }
    }

    /// Register this BIBE instance with the BPA.
    ///
    /// This registers the CLA and decapsulation service with the BPA,
    /// and adds all configured tunnel destinations.
    ///
    /// # Example
    /// ```ignore
    /// let config = bibe::Config::new(Eid::new("ipn:1.0"))
    ///     .with_decap_service_id(Service::Ipn(12))
    ///     .with_tunnel(NodeId::new("dtn://tunnel1"), Eid::new("ipn:100.12"));
    ///
    /// let bibe = Arc::new(bibe::Bibe::new(&config));
    /// bibe.register(&bpa).await?;
    /// ```
    pub async fn register(self: &Arc<Self>, bpa: &dyn BpaRegistration) -> Result<(), Error> {
        // Register CLA (uses Private address type)
        bpa.register_cla("bibe".into(), None, self.cla.clone(), None)
            .await?;

        // Register decapsulation service
        bpa.register_service(self.decap_service_id.clone(), self.decap_service.clone())
            .await?;

        // Register configured tunnel destinations
        let tunnel_count = self.tunnels.len();
        for tunnel in &self.tunnels {
            self.cla
                .add_tunnel(tunnel.tunnel.clone(), tunnel.decap_endpoint.clone())
                .await?;
        }

        debug!("BIBE initialized with {tunnel_count} tunnels");
        Ok(())
    }

    /// Unregister this BIBE instance from the BPA.
    pub async fn unregister(&self) {
        self.cla.unregister().await;
        self.decap_service.unregister().await;
    }

    /// Register a tunnel destination (creates virtual peer).
    ///
    /// This registers a "virtual peer" with the CLA, enabling routes
    /// to use `via <tunnel_id>` to tunnel traffic through BIBE.
    ///
    /// The `tunnel_id` NodeId becomes routable, and bundles forwarded to it
    /// will be encapsulated with `decap_endpoint` as the outer destination.
    ///
    /// # Example
    /// ```ignore
    /// // Register tunnel
    /// bibe.add_tunnel(
    ///     NodeId::new("dtn://tunnel1"),
    ///     Eid::new("ipn:100.12")
    /// ).await?;
    ///
    /// // Now routes can use: ipn:200.* via dtn://tunnel1
    /// ```
    pub async fn add_tunnel(&self, tunnel_id: NodeId, decap_endpoint: Eid) -> Result<(), Error> {
        self.cla.add_tunnel(tunnel_id, decap_endpoint).await
    }
}

/// Errors from BIBE operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// CLA not registered with BPA yet.
    #[error("CLA not registered with BPA")]
    NotRegistered,
    /// Invalid CLA address format.
    #[error("Invalid CLA address format")]
    InvalidAddress,
    /// Failed to parse bundle.
    #[error(transparent)]
    BundleParse(#[from] hardy_bpv7::Error),
    /// Failed to build bundle.
    #[error(transparent)]
    BundleBuild(#[from] hardy_bpv7::builder::Error),
    /// Failed to encode/decode CBOR.
    #[error(transparent)]
    Cbor(#[from] hardy_cbor::decode::Error),
    /// Failed to dispatch bundle.
    #[error(transparent)]
    Dispatch(#[from] hardy_bpa::cla::Error),
    /// Failed to register service with BPA.
    #[error(transparent)]
    ServiceRegistration(#[from] hardy_bpa::services::Error),
}
