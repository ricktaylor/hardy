use std::path::PathBuf;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct SessionConfig {
    // Seconds to wait for the initial contact header
    pub contact_timeout: u16, // default 15

    // Keepalive interval in seconds
    pub keepalive_interval: Option<u16>, // default 60

    // Whether to use TLS for encrypting the connection
    pub use_tls: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            contact_timeout: 15,
            keepalive_interval: Some(60),
            use_tls: true,
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct TlsConfig {
    // Required only if acting as a TLS server (listening for incoming connections)
    pub server_cert: Option<PathBuf>,   

    // Path to server private key file (PEM format)
    pub server_key: Option<PathBuf>,

    // Required only if acting as a TLS client (connecting to remote servers)
    // Path to directory containing CA certificate files (all .crt/.pem files in the directory will be loaded)
    pub ca_bundle: Option<PathBuf>,
    
    // Debug options (development only)
    #[cfg_attr(feature = "serde", serde(default))]
    pub debug: TlsDebugConfig,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct TlsDebugConfig {
    // Accept self-signed certificates when no CA is configured (for testing)
    pub accept_self_signed: bool,
}

impl Default for TlsDebugConfig {
    fn default() -> Self {
        Self {
            accept_self_signed: false,
        }
    }
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            server_cert: None,
            server_key: None,
            ca_bundle: None,
            debug: Default::default(),
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    // The TCP address:port to listen for TCP connections
    pub address: Option<std::net::SocketAddr>, // default = [::]:4556

    // Largest allowable single-segment data payload size to be received
    pub segment_mru: u64,

    // Largest allowable total-bundle data size to be received
    pub transfer_mru: u64,

    // Maximum number of idle connections, per remote address
    pub max_idle_connections: usize,

    #[cfg_attr(feature = "serde", serde(flatten))]
    pub session_defaults: SessionConfig,

    // TLS configuration (only used if use_tls is true)
    #[cfg_attr(feature = "serde", serde(default))]
    pub tls: TlsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            address: Some(std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
                4556,
            )),
            segment_mru: 16384,
            transfer_mru: 0x2_0000_0000_0000,
            max_idle_connections: 6,
            session_defaults: Default::default(),
            tls: Default::default(),
        }
    }
}
