use std::path::PathBuf;

/// Per-session parameters for TCPCLv4 connections (RFC 9174 Section 4).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct SessionConfig {
    /// Seconds to wait for the peer's contact header before aborting.
    /// RFC 9174 Section 4.2 recommends at most 60 seconds. Default: `15`.
    pub contact_timeout: u16,

    /// Keepalive interval in seconds, or `None`/`Some(0)` to disable.
    /// RFC 9174 Section 5.1.1 recommends 30..=600 on shared networks. Default: `Some(60)`.
    pub keepalive_interval: Option<u16>,

    /// When `true`, refuse sessions that do not negotiate TLS.
    /// Default: `false`.
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

/// TLS configuration for TCPCLv4 connections (RFC 9174 Section 4.4).
///
/// When present, enables TLS negotiation during the contact header exchange.
/// All paths default to `None`.
#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct TlsConfig {
    /// Path to the local certificate file (PEM). Required for the TLS server role.
    pub cert_file: Option<PathBuf>,

    /// Path to the local private key file (PEM). Required for the TLS server role.
    pub private_key_file: Option<PathBuf>,

    /// Path to a directory of CA certificates (`.crt`/`.pem`). Used to verify
    /// the peer's certificate when connecting as a TLS client.
    pub ca_certs: Option<PathBuf>,

    /// Override the TLS SNI server name (useful when connecting by IP address
    /// to a host whose certificate is issued for a domain name).
    pub server_name: Option<String>,

    // TODO(mTLS): Client certificate and key for mutual TLS authentication
    // pub client_cert_file: Option<PathBuf>,
    // pub client_key_file: Option<PathBuf>,
    /// Debug/development TLS options. Default: all disabled.
    pub debug: TlsDebugConfig,
}

/// Debug-only TLS options. Not intended for production use.
#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct TlsDebugConfig {
    /// Accept self-signed certificates when no CA is configured. Default: `false`.
    pub accept_self_signed: bool,
}

/// Top-level configuration for the TCPCLv4 CLA.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    /// TCP address and port to listen on, or `None` to disable the listener.
    /// Default: `[::]:4556` (RFC 9174 Section 3.1).
    pub address: Option<std::net::SocketAddr>,

    /// Maximum receivable segment payload size in bytes (RFC 9174 Section 4.3).
    /// Default: `16384`.
    pub segment_mru: u64,

    /// Maximum receivable total transfer (bundle) size in bytes (RFC 9174 Section 4.3).
    /// Default: `0x4000_0000` (1 GiB).
    pub transfer_mru: u64,

    /// Maximum number of idle connections retained per remote address. Default: `6`.
    pub max_idle_connections: usize,

    /// Maximum inbound connection rate in connections per second. Default: `64`.
    pub connection_rate_limit: u32,

    /// Session-level defaults (contact timeout, keepalive, TLS requirement).
    /// Flattened into the parent when deserialised with serde.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub session_defaults: SessionConfig,

    /// Optional TLS configuration. When `None`, all connections are unencrypted.
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
