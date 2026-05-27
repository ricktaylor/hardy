use super::*;
use thiserror::Error;

pub mod context;
pub(crate) mod peers;
pub(crate) mod registry;

mod egress_queue;

pub use context::ClaContext;

/// A specialized `Result` type for CLA operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during CLA operations.
#[derive(Debug, Error)]
pub enum Error {
    /// An attempt was made to register a CLA with a name that is already in use.
    #[error("Attempt to register duplicate CLA name {0}")]
    AlreadyExists(String),

    /// The connection to the BPA has been lost.
    #[error("Disconnected from BPA")]
    Disconnected,

    /// An internal error occurred.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

/// An enumeration of known CLA address types.
///
/// This is used to identify the protocol associated with a `ClaAddress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ClaAddressType {
    /// IPv4 and IPv6 address + port.
    Tcp,
    /// A private address type.
    Private,
}

/// Represents a network address for a specific Convergence Layer Adapter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ClaAddress {
    /// An TCP address, represented as a standard socket address.
    Tcp(core::net::SocketAddr),
    /// An address for an unknown or custom CLA, containing the type identifier and the raw address bytes.
    #[cfg_attr(feature = "serde", serde(with = "private_addr_serde"))]
    Private(Bytes),
}

#[cfg(feature = "serde")]
mod private_addr_serde {
    use super::Bytes;
    use base64::prelude::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        BASE64_URL_SAFE_NO_PAD.encode(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        let s = String::deserialize(d)?;
        BASE64_URL_SAFE_NO_PAD
            .decode(&s)
            .map(|v| v.into())
            .map_err(serde::de::Error::custom)
    }
}

impl ClaAddress {
    /// Returns the `ClaAddressType` corresponding to this address.
    pub fn address_type(&self) -> ClaAddressType {
        match self {
            ClaAddress::Tcp(_) => ClaAddressType::Tcp,
            ClaAddress::Private(_) => ClaAddressType::Private,
        }
    }
}

impl TryFrom<(ClaAddressType, Bytes)> for ClaAddress {
    type Error = Error;

    fn try_from((addr_type, addr): (ClaAddressType, Bytes)) -> Result<Self> {
        match addr_type {
            ClaAddressType::Tcp => Ok(ClaAddress::Tcp(
                String::from_utf8(addr.into())
                    .map_err(|e| Error::Internal(Box::new(e)))?
                    .parse()
                    .map_err(|e| Error::Internal(Box::new(e)))?,
            )),
            ClaAddressType::Private => Ok(ClaAddress::Private(addr)),
        }
    }
}

impl From<ClaAddress> for (ClaAddressType, Bytes) {
    fn from(value: ClaAddress) -> Self {
        match value {
            ClaAddress::Tcp(socket_addr) => (
                ClaAddressType::Tcp,
                socket_addr.to_string().into_bytes().into(),
            ),
            ClaAddress::Private(bytes) => (ClaAddressType::Private, bytes),
        }
    }
}

impl core::fmt::Display for ClaAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ClaAddress::Tcp(socket_addr) => write!(f, "tcp:{socket_addr}"),
            ClaAddress::Private(bytes) => {
                write!(f, "private:{bytes:02x?}")
            }
        }
    }
}

/// The result of a bundle forwarding attempt by a CLA.
pub enum ForwardBundleResult {
    /// The bundle was successfully sent.
    Sent,
    /// The bundle could not be sent because the neighbor is no longer available.
    NoNeighbour,
}

/// The primary trait for a Convergence Layer Adapter (CLA).
///
/// A CLA is responsible for adapting the Bundle Protocol to a specific underlying
/// transport, such as TCP, UDP, or a custom link-layer protocol. It handles the
/// transmission and reception of bundles over its specific medium.
///
/// # Context Lifecycle
///
/// The CLA receives a [`ClaContext`] in [`on_register`](Self::on_register). The context
/// contains channel senders for dispatching received bundles and managing peers.
/// Clone and store it if you need it beyond initialization.
///
/// Dropping all clones of the context closes the channels, which the BPA detects
/// as disconnection. All peers from this CLA are automatically removed.
///
/// Two disconnection paths exist:
/// - **CLA-initiated**: CLA drops all ClaContext clones. BPA calls `on_unregister()`.
/// - **BPA-initiated**: BPA cancels the shutdown token. CLA should stop work and drop the context.
#[async_trait]
pub trait Cla: Send + Sync {
    /// Called when the CLA is first registered with the BPA.
    ///
    /// The `ctx` provides channel-based access to the BPA for dispatching
    /// received bundles and managing peers. Clone it if you need it beyond
    /// this call.
    ///
    /// # Arguments
    /// * `ctx` - Channel-based context for communicating with the BPA.
    /// * `node_ids` - The BPA's own node identifiers.
    async fn on_register(&self, ctx: ClaContext, node_ids: &[hardy_bpv7::eid::NodeId]);

    /// Called when the CLA is being unregistered.
    ///
    /// This is called in two scenarios:
    /// 1. The CLA dropped all context clones (CLA-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    ///
    /// The CLA should perform cleanup: close connections, stop background tasks,
    /// and release resources.
    async fn on_unregister(&self);

    /// Returns the address type this CLA handles, if any.
    fn address_type(&self) -> Option<ClaAddressType> {
        None
    }

    /// Returns the number of egress queues this policy manages.
    /// The default is 0, for simple FIFO behavior.
    /// Any value > 0 indicates multiple priority queues with 0 highest
    ///
    /// If a CLA implements more than one queue, it is expected to implement strict priority.
    /// This means it will always transmit all packets from the highest priority queue (e.g., Queue 0)
    /// before servicing the next one (Queue 1), ensuring minimal latency for critical traffic
    fn queue_count(&self) -> u32 {
        0
    }

    /// Forwards a bundle to a specific CLA address over a given queue.
    ///
    /// Queue 'None' is the lowest priority Best Effort queue, often the only queue.
    async fn forward(
        &self,
        queue: Option<u32>,
        cla_addr: &ClaAddress,
        bundle: Bytes,
    ) -> Result<ForwardBundleResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ClaAddress round-trips through (ClaAddressType, Bytes) conversion.
    #[test]
    fn test_address_parsing() {
        // TCP address: parse from string representation
        let tcp_addr: core::net::SocketAddr = "192.168.1.1:4556".parse().unwrap();
        let cla_addr = ClaAddress::Tcp(tcp_addr);
        assert_eq!(cla_addr.address_type(), ClaAddressType::Tcp);

        // Round-trip: ClaAddress -> (type, bytes) -> ClaAddress
        let (addr_type, bytes): (ClaAddressType, Bytes) = cla_addr.clone().into();
        assert_eq!(addr_type, ClaAddressType::Tcp);
        let recovered = ClaAddress::try_from((addr_type, bytes)).unwrap();
        assert_eq!(recovered, cla_addr);

        // IPv6 TCP address
        let tcp_v6: core::net::SocketAddr = "[::1]:4556".parse().unwrap();
        let cla_v6 = ClaAddress::Tcp(tcp_v6);
        let (t, b): (ClaAddressType, Bytes) = cla_v6.clone().into();
        let recovered = ClaAddress::try_from((t, b)).unwrap();
        assert_eq!(recovered, cla_v6);

        // Private address
        let private_data = Bytes::from_static(b"\x01\x02\x03\x04");
        let private_addr = ClaAddress::Private(private_data.clone());
        assert_eq!(private_addr.address_type(), ClaAddressType::Private);

        let (t, b): (ClaAddressType, Bytes) = private_addr.clone().into();
        assert_eq!(t, ClaAddressType::Private);
        let recovered = ClaAddress::try_from((t, b)).unwrap();
        assert_eq!(recovered, private_addr);

        // Invalid TCP bytes should error
        let bad_bytes = Bytes::from_static(b"not-a-socket-addr");
        let result = ClaAddress::try_from((ClaAddressType::Tcp, bad_bytes));
        assert!(result.is_err());
    }
}
