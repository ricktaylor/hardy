use bytes::Bytes;
use hardy_async::async_trait;
use hardy_bpv7::eid::NodeId;

use super::{ClaAddress, Result};

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
///
/// # Sink Lifecycle
///
/// The CLA receives a [`Sink`] in [`on_register`](Self::on_register) which it **must store**
/// for its entire active lifetime. The Sink provides the communication channel back to the BPA.
///
/// **Critical**: If the Sink is dropped (either explicitly or by not storing it), the BPA
/// interprets this as the CLA requesting disconnection and will call [`on_unregister`](Self::on_unregister).
/// This means `on_register` must store the Sink before returning.
///
/// Two disconnection paths exist:
/// - **CLA-initiated**: CLA drops its Sink or calls `sink.unregister()` → BPA calls `on_unregister()`
/// - **BPA-initiated**: BPA shuts down → calls `on_unregister()` → Sink becomes non-functional
///
/// # Example
///
/// ```ignore
/// struct MyCla {
///     inner: Once<ClaInner>,
/// }
///
/// struct ClaInner {
///     sink: Arc<dyn Sink>,  // Stored for CLA lifetime
/// }
///
/// impl Cla for MyCla {
///     async fn on_register(&self, sink: Box<dyn Sink>, node_ids: &[NodeId]) {
///         self.inner.call_once(|| ClaInner { sink: sink.into() });
///     }
///     // ...
/// }
/// ```
#[async_trait]
pub trait Cla: Send + Sync {
    /// Called when the CLA is first registered with the BPA.
    ///
    /// The CLA should perform any necessary initialization, such as opening sockets
    /// or starting listener tasks.
    ///
    /// **Important**: The `sink` must be stored for the CLA's entire active lifetime.
    /// Dropping the sink triggers automatic unregistration. Convert to `Arc` for sharing:
    /// `let sink: Arc<dyn Sink> = sink.into();`
    ///
    /// # Arguments
    /// * `sink` - Communication channel back to the BPA. Must be stored.
    /// * `node_ids` - The BPA's own node identifiers.
    async fn on_register(&self, sink: Box<dyn ClaSink>, node_ids: &[NodeId]);

    /// Called when the CLA is being unregistered.
    ///
    /// This is called in two scenarios:
    /// 1. The CLA dropped its Sink (CLA-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    ///
    /// The CLA should perform cleanup: close connections, stop background tasks,
    /// and release resources. After this returns, the Sink is no longer functional.
    async fn on_unregister(&self);

    /// Returns the number of egress queues this CLA manages.
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
///
/// # Lifecycle
///
/// The Sink is provided to the CLA in [`Cla::on_register`]. The CLA **must store** this
/// Sink for its entire active lifetime. When the Sink is dropped, the BPA interprets
/// this as the CLA requesting disconnection.
///
/// Two disconnection paths exist:
/// - **CLA drops Sink**: BPA detects the drop and calls [`Cla::on_unregister`]
/// - **BPA shuts down**: BPA calls [`Cla::on_unregister`], then Sink operations return [`Error::Disconnected`]
///
/// After disconnection, all Sink operations return [`Error::Disconnected`].
#[async_trait]
pub trait ClaSink: Send + Sync {
    /// Explicitly unregisters the associated CLA from the BPA.
    ///
    /// This is equivalent to dropping the Sink, but allows explicit cleanup timing.
    /// After calling this, the BPA will call [`Cla::on_unregister`] and this Sink
    /// becomes non-functional.
    ///
    /// Typically called when the CLA encounters a fatal error and needs to shut down.
    async fn unregister(&self);

    /// Dispatches a received bundle (as raw bytes) to the BPA's `Dispatcher` for processing.
    ///
    /// The optional `peer_node` and `peer_addr` parameters provide ingress context:
    /// - `peer_node`: The node identifier of the peer that sent this bundle, if known
    ///   (e.g., learned during TCPCLv4 session establishment).
    /// - `peer_addr`: The convergence layer address of the peer, if applicable
    ///   (e.g., remote socket address for TCP-based CLAs).
    ///
    /// These may be `None` for CLAs without peer concepts (e.g., file-based) or
    /// unidirectional links.
    async fn dispatch(
        &self,
        bundle: Bytes,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&ClaAddress>,
    ) -> Result<()>;

    /// Notifies the BPA that a new peer (or neighbour) has been discovered at a given `ClaAddress`.
    ///
    /// The `node_ids` slice provides the BPA-layer identifiers for the peer:
    /// - An **empty slice** means the CLA has discovered a link-layer adjacency but does not yet
    ///   know the remote node's EID (a "Neighbour"). The BPA will record the address but will not
    ///   install a routing entry until the EID is resolved (e.g., via BP-ARP).
    /// - A **non-empty slice** means the CLA knows one or more EIDs for the peer (a "Peer").
    ///   Multi-homed nodes may have multiple EIDs at the same CL address.
    ///
    /// The BPA will update its routing information accordingly.
    async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> Result<bool>;

    /// Notifies the BPA that a peer is no longer reachable at a given `ClaAddress`.
    /// The BPA will update its routing information to remove all paths through this address.
    async fn remove_peer(&self, cla_addr: &ClaAddress) -> Result<bool>;
}
