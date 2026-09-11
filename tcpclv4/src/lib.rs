/*!
TCPCLv4 Convergence Layer Adapter for the Bundle Protocol.

This crate implements the TCP Convergence-Layer Protocol Version 4 (TCPCLv4)
as defined in [RFC 9174](https://www.rfc-editor.org/rfc/rfc9174). It provides
a [`Tcpclv4`] CLA that registers with the BPA to send and receive bundles
over TCP connections, with optional TLS encryption.

# Key types

- [`Tcpclv4`] — the convergence layer adapter, created via
  [`Tcpclv4::builder`] and registered with a BPA instance through
  `hardy_bpa`'s CLA registration.
- [`builder::Tcpclv4Builder`] — the fluent constructor; every setting has a
  default documented on its method.
- [`tls::Tls`] — TLS material (trust anchor, node identity, mutual-TLS
  policy), chained from [`tls::Tls::builder`] and loaded by
  [`builder::Tcpclv4Builder::build`].
- [`enum@Error`] — the generic error for everything TCPCLv4: assembling
  and registering the entity, with each sub-concept's error wrapped as a
  variant (`Tls`, `Session`).

# Feature flags

- `instrument` — adds `tracing` spans to the async internals.
- `serde` — adds `Serialize`/`Deserialize` impls to the invariant newtypes
  ([`ContactTimeout`], [`KeepaliveInterval`]), so consumer config schemas
  reject invalid values at deserialization.
*/

mod codec;
mod connection;
mod error;
mod otel_metrics;
mod session;
mod tcpclv4;
mod transport;
mod writer;

pub mod builder;
pub mod tls;

pub use self::error::Error;
pub use self::tcpclv4::Tcpclv4;

use core::num::{NonZeroU32, NonZeroU64};
use hardy_async::sync::spin::Once;
use hardy_bpv7::eid::NodeId;
use std::net::SocketAddr;
use std::sync::Arc;
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "instrument")]
use tracing::instrument;

/// Seconds to wait for a peer's contact header, bounded so that a value
/// outside RFC 9174 Section 4.2's recommendation (at most 60 seconds, and
/// never the instant timeout of zero) is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactTimeout(u16);

impl ContactTimeout {
    /// The RFC 9174 Section 4.2 recommended maximum: 60 seconds.
    pub const MAX: ContactTimeout = ContactTimeout(60);

    /// Creates a contact timeout; `None` when `seconds` is zero or above
    /// [`MAX`](Self::MAX).
    pub const fn new(seconds: u16) -> Option<Self> {
        if seconds == 0 || seconds > Self::MAX.0 {
            None
        } else {
            Some(Self(seconds))
        }
    }

    /// The timeout in whole seconds.
    pub const fn get(self) -> u16 {
        self.0
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for ContactTimeout {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.get().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for ContactTimeout {
    /// Deserializes from whole seconds, rejecting values the type cannot
    /// carry, so an invalid timeout fails at parse.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let seconds = u16::deserialize(deserializer)?;
        ContactTimeout::new(seconds).ok_or_else(|| {
            serde::de::Error::custom(
                "a contact timeout must be between 1 and 60 seconds (RFC 9174 Section 4.2)",
            )
        })
    }
}

/// Keepalive interval proposed during session negotiation, in seconds.
///
/// Zero is a first-class value ([`DISABLED`](Self::DISABLED)): RFC 9174
/// Section 4.7 encodes "KEEPALIVEs are disabled" as a zero interval on
/// the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepaliveInterval(u16);

impl KeepaliveInterval {
    /// Keepalives disabled: the wire's zero encoding (RFC 9174 Section 4.7).
    pub const DISABLED: KeepaliveInterval = KeepaliveInterval(0);

    /// Creates a keepalive interval; `0` is [`DISABLED`](Self::DISABLED).
    pub const fn new(seconds: u16) -> Self {
        Self(seconds)
    }

    /// The interval in whole seconds; `0` means keepalives are disabled.
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Whether keepalives are disabled.
    pub const fn is_disabled(self) -> bool {
        self.0 == 0
    }

    /// Negotiates the session keepalive against the peer's SESS_INIT
    /// proposal: the minimum of the two (RFC 9174 Section 4.7), where
    /// disabled (zero, on either side) wins.
    pub fn negotiate(self, peer_keepalive: u16) -> KeepaliveInterval {
        KeepaliveInterval(self.0.min(peer_keepalive))
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for KeepaliveInterval {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.get().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for KeepaliveInterval {
    /// Deserializes from whole seconds; every value is valid, with `0` as
    /// the wire's disabled encoding.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(KeepaliveInterval::new(u16::deserialize(deserializer)?))
    }
}

#[cfg(test)]
pub mod tests {
    // An ephemeral-port loopback address for test listeners: IPv6 when
    // available, IPv4 as a fallback (some sandboxes have no ::1). Probed
    // once per process. Shared by the other unit-test modules.
    pub fn loopback() -> std::net::SocketAddr {
        static IP: std::sync::OnceLock<std::net::IpAddr> = std::sync::OnceLock::new();
        std::net::SocketAddr::new(
            *IP.get_or_init(|| {
                if std::net::TcpListener::bind(("::1", 0)).is_ok() {
                    std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                } else {
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                }
            }),
            0,
        )
    }
}
