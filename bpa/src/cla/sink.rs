use hardy_bpv7::{bundle::Id, eid::NodeId};

use super::{Acceptance, Address, Result, Segment, TransferOutcome};
use crate::{async_trait, stream::Receiver};

/// The interface a CLA uses to reach the BPA.
///
/// A CLA hands received bundles over and announces the peers it discovers
/// through this trait, which keeps it independent of the BPA's internals.
///
/// # Lifecycle
///
/// The sink is supplied in the [`Context`](super::Context) a CLA is built
/// from. It stays usable until the CLA unregisters, either by calling
/// [`unregister`](Self::unregister) or because the BPA is shutting down,
/// after which every method returns
/// [`Error::Disconnected`](super::Error::Disconnected).
#[async_trait]
pub trait Sink: Send + Sync {
    /// Unregisters the CLA from the BPA, typically after a fault the CLA
    /// cannot recover from.
    ///
    /// The BPA calls [`Cla::unregister`](super::Cla::unregister) and
    /// withdraws every peer the CLA published. This sink is disconnected
    /// once the call returns.
    async fn unregister(&self);

    /// Dispatches a received bundle to the BPA, reading it from `stream`
    /// as a sequence of segments.
    ///
    /// Returns the [`Acceptance`] verdict for the transfer:
    /// [`Accepted`](Acceptance::Accepted) means the CLA may acknowledge the
    /// transfer to its peer, and [`Refused`](Acceptance::Refused) means it
    /// must not.
    ///
    /// The BPA may stop pulling and drop its end of `stream` at any point,
    /// whether or not it has reached a verdict; a duplicate recognised from
    /// the headers, for example, needs no further bytes. A producer that
    /// sees [`SendError`](crate::stream::SendError) therefore stops pushing
    /// segments and takes the verdict from the return value, never from the
    /// stream closing.
    ///
    /// A producer that drops its sender before [`Segment::Final`]
    /// truncates the bundle, and the BPA answers
    /// [`Refused`](Acceptance::Refused), so that the CLA withholds its
    /// acknowledgement and the peer can retransmit.
    ///
    /// `peer_node` is the sender's node identifier where the convergence
    /// layer has learned it, as a TCPCLv4 session does at establishment,
    /// and `peer_addr` the address the bundle arrived from. Both are `None`
    /// for a CLA with no notion of a peer, such as a file-based one.
    ///
    /// A caller holding a complete bundle in memory passes it as a
    /// single-segment stream, since `Bytes` implements [`Receiver`]:
    ///
    /// ```ignore
    /// sink.dispatch(peer_node, peer_addr, &mut bundle).await
    /// ```
    ///
    /// # Errors
    ///
    /// An error is never a verdict on the bundle, only a fault that makes
    /// the sink unusable: the CLA has unregistered, or the BPA has gone
    /// away.
    async fn dispatch(
        &self,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&Address>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<Acceptance>;

    /// Notifies the BPA that a peer is reachable at `cla_addr`, returning
    /// `true` if the address was not already known.
    ///
    /// `node_ids` holds the BPA-layer identifiers of the peer. An empty
    /// slice is a link-layer adjacency whose remote EID the CLA has not
    /// learned: the BPA records the adjacency but installs no route, and
    /// nothing supplies the EID later. A non-empty slice lists every EID
    /// the peer is known under, such as an `ipn` and a `dtn` one, and the
    /// BPA routes all of them through this address.
    async fn add_peer(&self, cla_addr: Address, node_ids: &[NodeId]) -> Result<bool>;

    /// Notifies the BPA that a peer is no longer reachable at `cla_addr`
    /// and withdraws every route through that address. Returns `true` if
    /// the address was known.
    async fn remove_peer(&self, cla_addr: &Address) -> Result<bool>;

    /// Reports the final outcome of a transfer previously answered
    /// [`ForwardOutcome::Accepted`](super::ForwardOutcome::Accepted).
    ///
    /// Every accepted transfer resolves exactly once, as `Completed`, as
    /// `Failed`, or implicitly as unknown when the CLA unregisters or the
    /// peer is removed. An outcome is applied only while the named bundle
    /// is still awaiting one through a peer of the reporting CLA; an
    /// outcome for a transfer that is already resolved, has expired, or
    /// belongs to another CLA is logged and discarded.
    async fn report_transfer(&self, bundle_id: &Id, outcome: TransferOutcome) -> Result<()>;
}
