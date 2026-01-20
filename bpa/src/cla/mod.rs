use super::*;
use std::collections::HashMap;
use thiserror::Error;

pub(crate) mod registry;

mod egress_queue;
mod peers;

/// A specialized `Result` type for CLA operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during CLA operations.
#[derive(Debug, Error)]
pub enum Error {
    /// An attempt was made to register a CLA with a name that is already in use.
    #[error("Attempt to register duplicate CLA name {0}")]
    AlreadyExists(String),

    /// The connection to the BPA has been lost.
    #[error("The sink is disconnected")]
    Disconnected,

    /// An error occurred while processing a BPv7 bundle.
    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

    /// An internal error occurred.
    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// An enumeration of known CLA address types.
///
/// This is used to identify the protocol associated with a `ClaAddress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaAddressType {
    /// IPv4 and IPv6 address + port.
    Tcp,
    /// A private address type.
    Private,
}

/// Represents a network address for a specific Convergence Layer Adapter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClaAddress {
    /// An TCP address, represented as a standard socket address.
    Tcp(core::net::SocketAddr),
    /// An address for an unknown or custom CLA, containing the type identifier and the raw address bytes.
    Private(Bytes),
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

impl std::fmt::Display for ClaAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
/// CLAs are often wrapped by an [`EgressPolicy`] to add more complex behaviors like
/// rate limiting or prioritization.
#[async_trait]
pub trait Cla: Send + Sync {
    /// Called when the CLA is first registered.
    ///
    /// The CLA should perform any necessary initialization, such as opening sockets
    /// or starting listener tasks. It is given a `sink` to communicate back to the
    /// BPA (e.g., to dispatch received bundles or report peer changes) and a list
    /// of the BPA's own node EIDs.
    async fn on_register(&self, sink: Box<dyn Sink>, node_ids: &[hardy_bpv7::eid::NodeId]);

    /// Called when the CLA is being unregistered.
    ///
    /// The CLA should perform any necessary cleanup, such as closing connections,
    /// stopping background tasks, and releasing resources.
    async fn on_unregister(&self);

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

/// A communication channel from a CLA back to the main BPA components.
///
/// This trait provides an abstraction that allows a CLA to be decoupled from the
/// internal implementation of the BPA. It provides a stable interface for a CLA to
/// dispatch incoming bundles and manage peer connections without needing direct access
/// to the BPA internals.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the associated CLA from the BPA. This is typically called by the CLA itself
    /// if it encounters a fatal error and needs to shut down.
    async fn unregister(&self);

    /// Dispatches a received bundle (as raw bytes) to the BPA's `Dispatcher` for processing.
    async fn dispatch(&self, bundle: Bytes) -> Result<()>;

    /// Notifies the BPA that a new peer has been discovered at a given `ClaAddress`.
    /// The BPA will update its routing information accordingly.
    async fn add_peer(
        &self,
        node_id: hardy_bpv7::eid::NodeId,
        cla_addr: ClaAddress,
    ) -> Result<bool>;

    /// Notifies the BPA that a peer is no longer reachable at a given `ClaAddress`.
    /// The BPA will update its routing information to remove the path.
    async fn remove_peer(
        &self,
        node_id: hardy_bpv7::eid::NodeId,
        cla_addr: &ClaAddress,
    ) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: Implement test for 'Address Parsing' (Verify ClaAddress conversion logic)
    #[test]
    fn test_address_parsing() {
        todo!("Verify ClaAddress conversion logic");
    }
}
