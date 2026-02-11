#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

mod cla;
mod config;
mod service;

pub use config::{Config, Tunnel};

// Common imports for submodules (accessed via `use super::*;`)
use alloc::{borrow::Cow, boxed::Box, sync::Arc, vec::Vec};
use hardy_async::sync::spin::Once;
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
    cla: Arc<cla::BibeCla>,
    decap_service: Arc<service::DecapService>,
}

impl Bibe {
    /// Create a new BIBE instance with the given tunnel source EID.
    ///
    /// For full initialization including BPA registration, use [`init()`] instead.
    pub fn new(tunnel_source: Eid) -> Self {
        let cla = Arc::new(cla::BibeCla::new(tunnel_source));
        let decap_service = Arc::new(service::DecapService::new(cla.clone()));

        Self { cla, decap_service }
    }

    /// Get the CLA for registration with BPA.
    ///
    /// Register with: `bpa.register_cla("bibe", bibe.cla()).await?;`
    pub fn cla(&self) -> Arc<dyn hardy_bpa::cla::Cla> {
        self.cla.clone()
    }

    /// Get the decap service for registration with BPA.
    ///
    /// Register with: `bpa.register_service(Some(service_id), bibe.decap_service()).await?;`
    pub fn decap_service(&self) -> Arc<dyn hardy_bpa::services::Service> {
        self.decap_service.clone()
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

/// Initialize BIBE and register with the BPA.
///
/// This creates a BIBE instance, registers the CLA and decapsulation service
/// with the BPA, and adds all configured tunnel destinations.
///
/// # Example
/// ```ignore
/// let config = bibe::Config::new(Eid::new("ipn:1.0"))
///     .with_decap_service_id(Service::Ipn(12))
///     .with_tunnel(NodeId::new("dtn://tunnel1"), Eid::new("ipn:100.12"));
///
/// let bibe = bibe::init(config, &bpa).await?;
/// ```
pub async fn init(config: Config, bpa: &hardy_bpa::bpa::Bpa) -> Result<Arc<Bibe>, Error> {
    let bibe = Arc::new(Bibe::new(config.tunnel_source.clone()));

    // Register CLA (uses Private address type)
    bpa.register_cla("bibe".into(), None, bibe.cla(), None)
        .await?;

    // Register decapsulation service
    bpa.register_service(config.decap_service_id, bibe.decap_service())
        .await?;

    // Register configured tunnel destinations
    let tunnel_count = config.tunnels.len();
    for tunnel in config.tunnels {
        bibe.add_tunnel(tunnel.tunnel, tunnel.decap_endpoint)
            .await?;
    }

    debug!("BIBE initialized with {tunnel_count} tunnels");
    Ok(bibe)
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
