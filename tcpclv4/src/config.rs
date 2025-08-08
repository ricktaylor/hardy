#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct SessionConfig {
    // Seconds to wait for the initial contact header
    pub contact_timeout: u16, // default 15

    // Keepalive interval in seconds
    pub keepalive_interval: Option<u16>, // default 60

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
pub struct Config {
    // The TCP address:port to listen for TCP connections
    pub address: std::net::SocketAddr, // default = [::]:4556

    // Largest allowable single-segment data payload size to be received
    pub segment_mru: u64,

    // Largest allowable total-bundle data size to be received
    pub transfer_mru: u64,

    // Maximum number of idle connections, per remote address
    pub max_idle_connections: usize,

    #[cfg_attr(feature = "serde", serde(flatten))]
    pub session_defaults: SessionConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            address: std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
                4556,
            ),
            segment_mru: 16384,
            transfer_mru: 0x2_0000_0000_0000,
            max_idle_connections: 6,
            session_defaults: Default::default(),
        }
    }
}
