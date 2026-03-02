use std::path::PathBuf;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct SessionConfig {
    // Seconds to wait for the initial contact header
    pub contact_timeout: u16, // default 15

    // Keepalive interval in seconds
    pub keepalive_interval: Option<u16>, // default 60

    // Whether to enforce TLS for encrypting the connection
    pub require_tls: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            contact_timeout: 15,
            keepalive_interval: Some(60),
            require_tls: false,
        }
    }
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct TlsConfig {
    // Path to the local certificate file (PEM). Required when acting as a TLS server.
    pub cert_file: Option<PathBuf>,

    // Path to the local private key file (PEM). Required when acting as a TLS server.
    pub private_key_file: Option<PathBuf>,

    // Path to directory containing CA certificate files (all .crt/.pem files loaded).
    // Required only if acting as a TLS client (connecting to remote servers).
    pub ca_certs: Option<PathBuf>,

    // Optional server name for TLS SNI (overrides IP-based logic).
    // Use this when connecting via IP but the certificate is issued for a domain name.
    pub server_name: Option<String>,

    // TODO(mTLS): Client certificate and key for mutual TLS authentication
    // pub client_cert_file: Option<PathBuf>,
    // pub client_key_file: Option<PathBuf>,

    // Debug options (development only)
    pub debug: TlsDebugConfig,
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct TlsDebugConfig {
    // Accept self-signed certificates when no CA is configured (for testing)
    pub accept_self_signed: bool,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    // The TCP address:port to listen for TCP connections
    pub address: Option<std::net::SocketAddr>, // default = [::]:4556

    // Largest allowable single-segment data payload size to be received
    pub segment_mru: u64,

    // Largest allowable total-bundle data size to be received
    pub transfer_mru: u64,

    // Maximum number of idle connections, per remote address
    pub max_idle_connections: usize,

    // Maximum incoming connection rate (connections per second)
    pub connection_rate_limit: u32,

    #[cfg_attr(feature = "serde", serde(flatten))]
    pub session_defaults: SessionConfig,

    // TLS configuration
    pub tls: Option<TlsConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            address: Some(std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
                4556,
            )),
            segment_mru: 16384,
            transfer_mru: 0x4000_0000, // 1GB
            max_idle_connections: 6,
            connection_rate_limit: 64,
            session_defaults: Default::default(),
            tls: Default::default(),
        }
    }
}
