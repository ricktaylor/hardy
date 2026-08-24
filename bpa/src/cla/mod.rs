use super::*;
use hardy_bpv7::bundle::Id;
use thiserror::Error;

pub use crate::stream::Segment;

pub(crate) mod peers;
pub(crate) mod registry;

mod egress_queue;

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

    /// The bundle stream ended before its final segment: the producer went
    /// away mid-bundle, the partial bytes are discarded, and the transfer
    /// must not be acknowledged to the peer.
    #[error("The bundle stream was cancelled before completion")]
    StreamCancelled,

    /// The bundle exceeds the transport's maximum message size and was
    /// rejected before being sent. Returned by transport-backed sinks
    /// (e.g. gRPC) when a pre-flight size check fails, instead of
    /// letting the oversized message break the underlying stream.
    #[error("Bundle too large: {size} bytes exceeds the maximum of {max} bytes")]
    PayloadTooLarge { size: usize, max: usize },

    /// A bundle stream completed with fewer bytes than its declared
    /// `total_len`. A transport may frame the wire transfer from the
    /// declared length before pulling the first segment, so a short
    /// delivery is rejected rather than shorting the transfer on the wire.
    #[error("Bundle stream delivered {size} bytes of the {expected} declared")]
    PayloadUnderrun { size: usize, expected: usize },

    #[error("declared length of {total_len} bytes is unaddressable on this target")]
    PayloadUnaddressable { total_len: u64 },

    /// An internal error occurred.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

impl From<crate::stream::BufferError> for Error {
    fn from(e: crate::stream::BufferError) -> Self {
        match e {
            crate::stream::BufferError::Cancelled => Error::StreamCancelled,
            crate::stream::BufferError::TooLarge { size, max } => {
                Error::PayloadTooLarge { size, max }
            }
            crate::stream::BufferError::Underrun { size, expected } => {
                Error::PayloadUnderrun { size, expected }
            }
            crate::stream::BufferError::Unaddressable { total_len } => {
                Error::PayloadUnaddressable { total_len }
            }
        }
    }
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
    /// The CLA has taken ownership of the bundle and will report the
    /// transfer's outcome later via [`Sink::transfer_outcome`]. The BPA
    /// retains the bundle until the outcome arrives, the peer is removed,
    /// or the bundle's lifetime expires.
    Accepted,
    /// The bundle could not be sent because the neighbor is no longer available.
    NoNeighbour,
}

/// The final outcome of a transfer previously answered
/// [`ForwardBundleResult::Accepted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOutcome {
    /// The bundle was handed to the far bundle node.
    Completed,
    /// The convergence layer gave up on the transfer.
    ///
    /// Not proof of non-delivery: the far end may already hold the bundle
    /// (acknowledgement loss), and receiving-side deduplication absorbs the
    /// re-forward.
    Failed,
}

/// The primary trait for a Convergence Layer Adapter (CLA).
///
/// A CLA is responsible for adapting the Bundle Protocol to a specific underlying
/// transport, such as TCP, UDP, or a custom link-layer protocol. It handles the
/// transmission and reception of bundles over its specific medium.
///
/// CLAs are often wrapped by an [`EgressPolicy`](crate::policy::EgressPolicy)
/// to add more complex behaviors like rate limiting or prioritization.
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
    async fn on_register(&self, sink: Box<dyn Sink>, node_ids: &[hardy_bpv7::eid::NodeId]);

    /// Called when the CLA is being unregistered.
    ///
    /// This is called in two scenarios:
    /// 1. The CLA dropped its Sink (CLA-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    ///
    /// The CLA should perform cleanup: close connections, stop background tasks,
    /// and release resources. After this returns, the Sink is no longer functional.
    async fn on_unregister(&self);

    /// Returns the address type this CLA handles, if any.
    fn address_type(&self) -> Option<ClaAddressType> {
        None
    }

    /// Returns this CLA's lane count: its honest parallelism, the number
    /// of transfers it can usefully carry in flight at once.
    ///
    /// `None` declares no limit: the CLA is effectively unconstrained (a
    /// datagram CL), and every forward arrives with lane `None`, each
    /// transfer travelling on a new lane from the infinite pool. `Some(n)`
    /// declares `n` explicit lanes, indexed `0..n`; a zero count is
    /// unrepresentable by construction. There is deliberately no default:
    /// parallelism is a property every CLA must state for itself.
    ///
    /// Lanes are parallel transport channels — QUIC streams, DSCP classes,
    /// separate TCP connections — carrying in-flight transfers with no
    /// explicit priority among them. Scheduling and prioritisation live in
    /// the BPA's egress policy, which decides what is forwarded onto each
    /// lane; the CLA simply transmits what arrives on a lane, and one
    /// lane's in-flight transfer must not head-of-line block another's.
    fn lane_count(&self) -> Option<core::num::NonZeroU32>;

    /// Forwards a bundle, delivered as a stream of segments, to a specific CLA
    /// address over a given lane.
    ///
    /// Lane `None` requests a new lane from the infinite pool — the
    /// default for CLAs that declare no lane limit, where every transfer
    /// travels on its own fresh lane (a datagram CL). `Some(i)` directs
    /// the transfer onto explicit lane `i`. As a safety net, a CLA that
    /// declares explicit lanes but still receives `None` should pick a
    /// lane arbitrarily (e.g. at random) rather than fail the transfer.
    /// Mirroring [`Sink::dispatch`] on the egress side, the
    /// implementation pulls [`Segment::Next`] items from `stream` until
    /// [`Segment::Final`] (which may carry empty bytes) completes the
    /// transfer.
    ///
    /// A pull returning `Err(`[`RecvError`](crate::stream::RecvError)`)`
    /// before `Final` means the producer aborted the transfer: the CLA must
    /// tear down without delivering a partial bundle to the peer, and must
    /// not return `Ok` — a truncated transfer surfaced as success would let
    /// the peer treat delivery as complete.
    ///
    /// `total_len` is the exact number of bundle bytes the stream will
    /// deliver — the sum of all segment payload lengths. It is a framing
    /// hint for transports that must announce a length up front (e.g. a
    /// TCPCLv4 XFER_SEGMENT length), carried as `u64` so it remains valid on
    /// 32-bit targets. Producers must be exact: an implementation may frame
    /// the wire transfer from it before pulling the first segment.
    ///
    /// `bundle_id` identifies the transfer if the CLA defers its outcome: a
    /// CLA answering [`ForwardBundleResult::Accepted`] echoes it back in
    /// [`Sink::transfer_outcome`]. It is the correlation key, not data to
    /// transmit — CLAs that answer terminally may ignore it, and no CLA needs
    /// to parse the bundle to learn it.
    ///
    /// An implementation that needs the whole bundle in memory buffers the
    /// stream with [`stream::buffer_stream`](crate::stream::buffer_stream),
    /// whose errors convert into this module's [`Error`] via `?`.
    async fn forward(
        &self,
        lane: Option<u32>,
        cla_addr: &ClaAddress,
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn crate::stream::Receiver<Segment>,
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
pub trait Sink: Send + Sync {
    /// Explicitly unregisters the associated CLA from the BPA.
    ///
    /// This is equivalent to dropping the Sink, but allows explicit cleanup timing.
    /// After calling this, the BPA will call [`Cla::on_unregister`] and this Sink
    /// becomes non-functional.
    ///
    /// Typically called when the CLA encounters a fatal error and needs to shut down.
    async fn unregister(&self);

    /// Dispatches a received bundle (as a stream of segments) to the BPA's `Dispatcher` for processing.
    ///
    /// A producer that drops its sender before [`Segment::Final`] has
    /// truncated the bundle; the implementation must surface an error
    /// (never a silent `Ok`), so the CLA withholds its transfer
    /// acknowledgement and the peer can retransmit.
    ///
    /// A caller holding a complete bundle in memory dispatches it as a
    /// one-segment stream, since `Bytes` implements [`stream::Receiver`](crate::stream::Receiver):
    /// `sink.dispatch(&mut bundle, ..).await`.
    ///
    /// Producers: a failed send into the stream means the consumer has given
    /// up on the transfer (size cap, dead registration, shutdown); stop
    /// streaming and discard.
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
        peer_node: Option<&hardy_bpv7::eid::NodeId>,
        peer_addr: Option<&ClaAddress>,
        stream: &mut dyn crate::stream::Receiver<Segment>,
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
    async fn add_peer(
        &self,
        cla_addr: ClaAddress,
        node_ids: &[hardy_bpv7::eid::NodeId],
    ) -> Result<bool>;

    /// Notifies the BPA that a peer is no longer reachable at a given `ClaAddress`.
    /// The BPA will update its routing information to remove all paths through this address.
    async fn remove_peer(&self, cla_addr: &ClaAddress) -> Result<bool>;

    /// Reports the final outcome of a transfer previously answered
    /// [`ForwardBundleResult::Accepted`].
    ///
    /// Every accepted transfer resolves exactly once: `Completed`, `Failed`,
    /// or implicitly outcome-unknown when the CLA unregisters or the peer is
    /// removed. An outcome is honoured only while the named bundle is still
    /// awaiting one via a peer of the reporting CLA; anything else — already
    /// resolved, expired, another CLA's transfer — is logged and dropped.
    async fn transfer_outcome(&self, bundle_id: &Id, outcome: TransferOutcome) -> Result<()>;
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
