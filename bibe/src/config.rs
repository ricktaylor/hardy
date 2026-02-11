use super::*;

/// Configuration for BIBE tunnel endpoints.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Config {
    /// Source EID for outer bundles created by encapsulation.
    pub tunnel_source: Eid,
    /// Service ID for the decapsulation service endpoint.
    /// If `None`, the BPA will auto-assign a service ID.
    #[cfg_attr(feature = "serde", serde(default))]
    pub decap_service_id: Option<Service>,
    /// Virtual peers to register as tunnel destinations.
    #[cfg_attr(feature = "serde", serde(default))]
    pub tunnels: Vec<Tunnel>,
}

/// A tunnel destination configuration.
///
/// Routes can reference this tunnel using `via <tunnel_id>`, e.g.:
/// ```text
/// ipn:200.* via dtn://tunnel1
/// ```
///
/// The `decap_endpoint` is the actual service endpoint (e.g., `ipn:100.12`)
/// that receives the outer bundle for decapsulation.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Tunnel {
    /// The tunnel NodeId that becomes routable (used in `via` routes).
    pub tunnel: NodeId,
    /// The decapsulation endpoint EID for the outer bundle destination.
    pub decap_endpoint: Eid,
}

impl Config {
    /// Create a new configuration with the given tunnel source EID.
    pub fn new(tunnel_source: Eid) -> Self {
        Self {
            tunnel_source,
            decap_service_id: None,
            tunnels: Vec::new(),
        }
    }

    /// Set the decapsulation service ID.
    pub fn with_decap_service_id(mut self, service_id: Service) -> Self {
        self.decap_service_id = Some(service_id);
        self
    }

    /// Add a tunnel destination to the configuration.
    pub fn with_tunnel(mut self, tunnel: NodeId, decap_endpoint: Eid) -> Self {
        self.tunnels.push(Tunnel {
            tunnel,
            decap_endpoint,
        });
        self
    }
}
