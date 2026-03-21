use std::net::SocketAddr;

/// Framing mode for the CLA.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Framing {
    /// MTCP: CBOR byte string framing (draft-ietf-dtn-mtcpcl-01).
    /// Used by D3TN/ud3tn.
    Mtcp,
    /// STCP: 4-byte big-endian u32 length prefix.
    /// Used by ION (actual wire format, not the STCP spec).
    Stcp,
}

/// Peer to register on startup (for static routing).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PeerConfig {
    /// TCP address of the peer (e.g., "127.0.0.1:4556")
    pub address: SocketAddr,
    /// Node ID of the peer (e.g., "ipn:2.0")
    pub node_id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    /// TCP address to listen on (e.g., "[::]:4556").
    /// If not set, the CLA will not accept incoming connections.
    pub address: Option<SocketAddr>,

    /// Framing mode: "mtcp" (CBOR byte string) or "stcp" (4-byte u32).
    pub framing: Framing,

    /// Maximum bundle size to accept (bytes). Default: 1GB.
    #[serde(default = "default_max_bundle_size")]
    pub max_bundle_size: u64,

    /// Peer address for outbound connections (e.g., "127.0.0.1:4556").
    /// Used by bp ping to specify the destination peer address.
    pub peer: Option<String>,

    /// Peer node ID (e.g., "ipn:2.0").
    /// When set with `peer`, the CLA calls sink.add_peer() on registration
    /// to create a wildcard RIB entry for routing.
    pub peer_node: Option<String>,
}

fn default_max_bundle_size() -> u64 {
    0x4000_0000 // 1GB
}
