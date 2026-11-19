use hardy_bpv7::{bundle::Id, eid::NodeId};

use super::{Acceptance, Address, Result, Segment, TransferOutcome};
use crate::{async_trait, stream::Receiver};

/// A communication channel from a CLA back to the main BPA components.
///
/// This trait provides an abstraction that allows a CLA to be decoupled from the
/// internal implementation of the BPA. It provides a stable interface for a CLA to
/// dispatch incoming bundles and manage peer connections without needing direct access
/// to the BPA internals.
///
/// # Lifecycle
///
/// The Sink is provided to the CLA in [`Cla::on_register`](super::Cla::on_register). The CLA **must store** this
/// Sink for its entire active lifetime. When the Sink is dropped, the BPA interprets
/// this as the CLA requesting disconnection.
///
/// Two disconnection paths exist:
/// - **CLA drops Sink**: BPA detects the drop and calls [`Cla::on_unregister`](super::Cla::on_unregister)
/// - **BPA shuts down**: BPA calls [`Cla::on_unregister`](super::Cla::on_unregister), then Sink operations return [`Error::Disconnected`](super::Error::Disconnected)
///
/// After disconnection, all Sink operations return [`Error::Disconnected`](super::Error::Disconnected).
#[async_trait]
pub trait Sink: Send + Sync {
    /// Explicitly unregisters the associated CLA from the BPA.
    ///
    /// This is equivalent to dropping the Sink, but allows explicit cleanup timing.
    /// After calling this, the BPA will call [`Cla::on_unregister`](super::Cla::on_unregister) and this Sink
    /// becomes non-functional.
    ///
    /// Typically called when the CLA encounters a fatal error and needs to shut down.
    async fn unregister(&self);

    /// Dispatches a received bundle, as a stream of segments, to the BPA.
    ///
    /// Returns the [`Acceptance`] verdict for the transfer — its only
    /// carrier: [`Accepted`](Acceptance::Accepted) means the CLA may
    /// acknowledge the transfer to its peer, [`Refused`](Acceptance::Refused)
    /// means it must not. `Err(_)` is reserved for faults that make the sink
    /// unusable (the registration or the BPA has gone away) — never for a
    /// verdict.
    ///
    /// The stream itself carries no verdict. The BPA may stop pulling — and
    /// drop its end of `stream` — at any point: it may already have decided
    /// to accept (a duplicate identified from the headers needs no further
    /// bytes) or not yet have decided anything. A producer that sees
    /// [`SendError`](crate::stream::SendError) must stop pushing segments and
    /// then take the verdict from `dispatch`'s return value, never from the
    /// stream closing.
    ///
    /// A producer that drops its sender before [`Segment::Final`] has
    /// truncated the bundle; the implementation must surface that as
    /// [`Refused`](Acceptance::Refused) (never a silent acceptance), so the
    /// CLA withholds its transfer acknowledgement and the peer can
    /// retransmit.
    ///
    /// A caller holding a complete bundle in memory dispatches it as a
    /// one-segment stream, since `Bytes` implements [`Receiver`]:
    /// `sink.dispatch(&mut bundle, ..).await`.
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
    /// The BPA will update its routing information accordingly.
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
    /// awaiting one via a peer of the reporting CLA; anything else — already
    /// resolved, expired, another CLA's transfer — is logged and dropped.
    async fn transfer_outcome(&self, bundle_id: &Id, outcome: TransferOutcome) -> Result<()>;
}
