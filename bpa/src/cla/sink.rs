use hardy_bpv7::{bundle::Id, eid::NodeId};

use super::{Acceptance, Address, Result, Segment, TransferOutcome};
use crate::{async_trait, stream::Receiver};

/// A CLA's channel back into the BPA.
///
/// The sink decouples a CLA from the BPA's internals: a stable interface
/// for handing received bundles over and for announcing the peers the
/// convergence layer discovers.
///
/// # Lifecycle
///
/// The sink arrives in the [`Context`](super::Context) a CLA is built
/// from, and stays usable until the CLA unregisters, by calling
/// [`unregister`](Self::unregister) or because the BPA is shutting down.
/// After that every operation here returns
/// [`Error::Disconnected`](super::Error::Disconnected).
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the CLA from the BPA.
    ///
    /// The BPA calls [`Cla::unregister`](super::Cla::unregister) and
    /// withdraws every peer the CLA published; this sink is dead from then
    /// on.
    /// Typically called when the CLA hits a fault it cannot recover from.
    async fn unregister(&self);

    /// Dispatches a received bundle, as a stream of segments, to the BPA.
    ///
    /// Returns the [`Acceptance`] verdict for the transfer, its only
    /// carrier: [`Accepted`](Acceptance::Accepted) means the CLA may
    /// acknowledge the transfer to its peer, [`Refused`](Acceptance::Refused)
    /// means it must not. `Err(_)` is reserved for faults that make the sink
    /// unusable (the CLA has unregistered, or the BPA has gone away), never
    /// a verdict.
    ///
    /// The stream itself carries no verdict. The BPA may stop pulling, and
    /// drop its end of `stream`, at any point: it may already have decided
    /// to accept (a duplicate identified from the headers needs no further
    /// bytes) or not yet have decided anything. A producer that sees
    /// [`SendError`](crate::stream::SendError) must stop pushing segments and
    /// then take the verdict from this method's return value, never from the
    /// stream closing.
    ///
    /// A producer that drops its sender before [`Segment::Final`] has
    /// truncated the bundle; the implementation must surface that as
    /// [`Refused`](Acceptance::Refused) (never a silent acceptance), so the
    /// CLA withholds its transfer acknowledgement and the peer can
    /// retransmit.
    ///
    /// A caller holding a complete bundle in memory passes it as a
    /// one-segment stream, since `Bytes` implements [`Receiver`]:
    /// `sink.dispatch(peer_node, peer_addr, &mut bundle).await`.
    ///
    /// `peer_node` and `peer_addr` carry what the CLA knows about the sender:
    /// the peer's node identifier, if the convergence layer learned it (a
    /// TCPCLv4 session exchanges it at establishment), and the address the
    /// bundle arrived from. Both are `None` for a CLA with no notion of a
    /// peer, such as a file-based or unidirectional one.
    async fn dispatch(
        &self,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&Address>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<Acceptance>;

    /// Notifies the BPA that a peer (or neighbour) is reachable at `cla_addr`.
    ///
    /// The `node_ids` slice provides the BPA-layer identifiers for the peer:
    /// - An **empty slice** means the CLA has discovered a link-layer adjacency but does not yet
    ///   know the remote node's EID (a "Neighbour"). The BPA records the adjacency but installs no
    ///   routing entry, and no interface yet supplies the EID later, so a neighbour stays unroutable.
    /// - A **non-empty slice** means the CLA knows one or more EIDs for the peer (a "Peer"). A node
    ///   known under several EIDs (an `ipn` and a `dtn` one, say) lists them all at its CL address.
    ///
    /// The BPA updates its routing information accordingly.
    async fn add_peer(&self, cla_addr: Address, node_ids: &[NodeId]) -> Result<bool>;

    /// Notifies the BPA that a peer is no longer reachable at `cla_addr`,
    /// withdrawing every route through that address.
    async fn remove_peer(&self, cla_addr: &Address) -> Result<bool>;

    /// Reports the final outcome of a transfer previously answered
    /// [`ForwardOutcome::Accepted`](super::ForwardOutcome::Accepted).
    ///
    /// Every accepted transfer resolves exactly once: `Completed`, `Failed`,
    /// or implicitly outcome-unknown when the CLA unregisters or the peer is
    /// removed. An outcome is honoured only while the named bundle is still
    /// awaiting one via a peer of the reporting CLA. Anything else (already
    /// resolved, expired, another CLA's transfer) is logged and dropped.
    async fn report_transfer(&self, bundle_id: &Id, outcome: TransferOutcome) -> Result<()>;
}
