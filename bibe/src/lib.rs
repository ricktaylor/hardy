/*!
Bundle-in-Bundle Encapsulation (BIBE) for the Hardy BPA.

This crate implements Bundle-in-Bundle Encapsulation (draft-ietf-dtn-bibect),
enabling bundles to be tunneled through intermediate DTN networks by wrapping an
inner bundle inside the payload of an outer bundle. It uses a hybrid CLA/Service
architecture: encapsulation is performed by a [`Cla`](hardy_bpa::cla::Cla)
implementation that intercepts `forward()` calls, while decapsulation is handled by
a [`raw::Service`](hardy_bpa::service::raw::Service) that receives outer bundles, extracts
the inner bundle, and re-injects it into the BPA. Tunnel destinations are registered
as virtual peers, making them routable via standard BPA forwarding.
*/

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::sync::Arc;

use hardy_bpa::{bpa::BpaRegistration, cla::RegistrarExt, service::RegistrarExt as _};
use hardy_bpv7::eid::{Eid, NodeId};
use thiserror::Error as ThisError;
use tracing::debug;

use crate::{cla::BibeCla, service::DecapService};

mod cla;
mod config;
mod pdu;
mod service;

pub use config::{Config, Tunnel};

/// BIBE tunnel endpoint manager.
///
/// Provides Bundle-in-Bundle Encapsulation for tunneling bundles
/// through intermediate networks. Uses a hybrid architecture:
/// - CLA for encapsulation (via `forward()`)
/// - Service for decapsulation (via `deliver()`)
pub struct Bibe {
    cla: Arc<BibeCla>,
    decap_service: Arc<DecapService>,
}

impl Bibe {
    /// Registers the CLA and the decapsulation service `config` describes
    /// with `bpa`, and adds every tunnel destination it lists.
    ///
    /// The BPA owns both components from here on; the handle returned is
    /// for adding further tunnels and for detaching.
    ///
    /// # Example
    /// ```ignore
    /// let config = bibe::Config::new(Eid::new("ipn:1.0"))
    ///     .with_decap_service_id(Service::Ipn(12))
    ///     .with_tunnel(NodeId::new("dtn://tunnel1"), Eid::new("ipn:100.12"));
    ///
    /// let bibe = bibe::Bibe::register(&config, &bpa).await?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if either registration is refused, or if a
    /// configured tunnel destination cannot be added as a peer.
    pub async fn register(config: &Config, bpa: &dyn BpaRegistration) -> Result<Self, Error> {
        // The CLA carries `Address::Private` peers and declares no receive
        // limit: the encapsulation overhead is per bundle, so the
        // outer-vs-limit check happens per forward instead.
        let cla = bpa
            .register_cla("bibe", BibeCla::factory(config.tunnel_source.clone()))
            .await?;

        let decap_service = bpa
            .register_raw_service(
                config.decap_service_id.clone(),
                DecapService::factory(cla.clone()),
            )
            .await?;

        for tunnel in &config.tunnels {
            cla.add_tunnel(tunnel.tunnel.clone(), tunnel.decap_endpoint.clone())
                .await?;
        }

        debug!("BIBE initialized with {} tunnels", config.tunnels.len());

        Ok(Self { cla, decap_service })
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
    ///
    /// # Errors
    ///
    /// Returns an error if the BPA refuses the virtual peer.
    pub async fn add_tunnel(&self, tunnel_id: NodeId, decap_endpoint: Eid) -> Result<(), Error> {
        self.cla.add_tunnel(tunnel_id, decap_endpoint).await
    }
}

/// Errors from BIBE operations.
///
/// The wrapped source types are named by path: every one of them is called
/// `Error`, so importing them would collide with each other and with this
/// enum.
#[derive(Debug, ThisError)]
pub enum Error {
    /// Invalid CLA address format.
    #[error("Invalid CLA address format")]
    InvalidAddress,
    /// The BIBE-PDU is a segment of a larger bundle; segmentation is not supported.
    #[error("Segmented BIBE-PDUs are not supported")]
    SegmentedPdu,
    /// Failed to parse bundle.
    #[error(transparent)]
    BundleParse(#[from] hardy_bpv7::Error),
    /// Failed to build bundle.
    #[error(transparent)]
    BundleBuild(#[from] hardy_bpv7::builder::Error),
    /// Failed to encode/decode CBOR.
    #[error(transparent)]
    Cbor(#[from] hardy_cbor::decode::Error),
    /// The BPA refused the encapsulated bundle.
    #[error("the BPA refused the encapsulated bundle")]
    Refused,
    /// Failed to dispatch bundle.
    #[error(transparent)]
    Dispatch(#[from] hardy_bpa::cla::Error),
    /// Failed to register service with BPA.
    #[error(transparent)]
    ServiceRegistration(#[from] hardy_bpa::service::Error),
}
