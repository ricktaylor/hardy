use core::num::{NonZeroU32, NonZeroU64};

use hardy_bpv7::{bundle::Id, eid::NodeId};
use thiserror::Error;

use crate::{
    async_trait,
    stream::{BufferError, Receiver},
};

mod address;
mod queue;
mod sink;

pub(crate) mod peers;
pub(crate) mod registry;

pub use crate::stream::Segment;
pub use address::{Address, AddressType};
pub use sink::Sink;

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
    /// away mid-bundle and the partial bytes are discarded.
    #[error("The bundle stream was cancelled before completion")]
    StreamCancelled,

    /// The bundle exceeds a size limit. Raised at the ingress door when
    /// a received stream grows past the BPA's `max_bundle_size`, and by
    /// transport-backed sinks (e.g. gRPC) when a pre-flight size check
    /// fails, instead of letting the oversized message break the
    /// underlying stream. The fields are wire-size-domain `u64`; today
    /// they carry in-memory byte counts.
    #[error("Bundle too large: {size} bytes exceeds the maximum of {max} bytes")]
    PayloadTooLarge { size: u64, max: u64 },

    /// A bundle stream completed with fewer bytes than its declared
    /// `total_len`. A transport may frame the wire transfer from the
    /// declared length before pulling the first segment, so a short
    /// delivery is rejected rather than shorting the transfer on the wire.
    /// The fields are wire-size-domain `u64`.
    #[error("Bundle stream delivered {size} bytes of the {expected} declared")]
    PayloadUnderrun { size: u64, expected: u64 },

    /// A stream declared a `total_len` too large to address in memory on
    /// this target: no buffer can hold it, so the stream is rejected
    /// before the first segment is pulled.
    #[error("declared length of {total_len} bytes is unaddressable on this target")]
    PayloadUnaddressable { total_len: u64 },

    /// An internal error occurred.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

impl From<BufferError> for Error {
    fn from(e: BufferError) -> Self {
        match e {
            BufferError::Cancelled => Error::StreamCancelled,
            BufferError::TooLarge { size, max } => Error::PayloadTooLarge {
                size: size as u64,
                max: max as u64,
            },
            BufferError::Underrun { size, expected } => Error::PayloadUnderrun {
                size: size as u64,
                expected: expected as u64,
            },
            BufferError::Unaddressable { total_len } => Error::PayloadUnaddressable { total_len },
        }
    }
}

/// The result of a bundle forwarding attempt by a CLA.
///
/// The variants encode who owns the transfer when `forward` returns, not
/// merely success or failure. An *unreliable* convergence layer is
/// fire-and-forget and answers terminally: [`Sent`](Self::Sent) is all it
/// will ever know. [`Accepted`](Self::Accepted) exists only for *reliable*
/// convergence layers that learn their peer's acceptance later: returning
/// early frees the egress lane while that acknowledgement is in flight, so
/// a session can pipeline many transfers concurrently. This deferred
/// callback is the pipelining seam — a `forward` that awaited the final
/// outcome would serialize every lane on a wire round-trip. Local delivery
/// has no analogue of `Accepted` because `on_deliver` completes in-process,
/// with no round-trip to wait out.
pub enum ForwardOutcome {
    /// The bundle was successfully sent, and no further outcome will be
    /// reported — the terminal, fire-and-forget answer of an unreliable
    /// convergence layer.
    Sent,
    /// The CLA has taken ownership of the bundle and will report the
    /// transfer's outcome later via [`Sink::transfer_outcome`]. The BPA
    /// retains the bundle until the outcome arrives or the peer is removed;
    /// a lifetime that expires mid-transfer is enforced when the outcome
    /// arrives, not by recalling the transfer.
    Accepted,
    /// The bundle could not be sent because the neighbor is no longer available.
    NoNeighbour,
}

/// The final outcome of a transfer previously answered
/// [`ForwardOutcome::Accepted`].
///
/// Today the outcome only resolves the retained bundle, but it is
/// deliberately the peer-acceptance signal a future reliability /
/// custody-transfer layer will consume — which is why reliable CLAs
/// report it even though `Completed` and a fire-and-forget
/// [`Sent`](ForwardOutcome::Sent) currently resolve identically.
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

/// A CLA's set-once registration declarations.
///
/// Passed to [`BpaRegistration::register_cla`](crate::bpa::BpaRegistration::register_cla)
/// alongside the CLA and sampled exactly once: the BPA snapshots every
/// field at registration, so none of them can change — or be re-read —
/// for the lifetime of the registration. A CLA with nothing to declare
/// registers with `ClaInit::default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaInit {
    /// The address type this CLA handles, if any. The BPA records it
    /// against the CLA, but no lookup consults the record.
    pub address_type: Option<AddressType>,

    /// The CLA's lane count: its honest parallelism, the number of
    /// transfers it can usefully carry in flight at once.
    ///
    /// `None` declares no limit: the CLA is effectively unconstrained (a
    /// datagram CL), and every forward arrives with lane `None`, each
    /// transfer travelling on a new lane from the infinite pool. `Some(n)`
    /// declares `n` explicit lanes, indexed `0..n`; a zero count is
    /// unrepresentable by construction.
    ///
    /// Lanes are parallel transport channels — QUIC streams, DSCP classes,
    /// separate TCP connections — carrying in-flight transfers with no
    /// explicit priority among them. Scheduling and prioritisation live in
    /// the BPA's egress policy, which decides what is forwarded onto each
    /// lane; the CLA simply transmits what arrives on a lane, and one
    /// lane's in-flight transfer must not head-of-line block another's.
    pub lane_count: Option<NonZeroU32>,

    /// The CLA's declared receive limit (`None` = no CLA-side limit).
    ///
    /// The BPA folds it with its own configured cap into the effective
    /// value delivered to [`Cla::on_register`]; dispatch enforces the
    /// BPA's configured cap, not the folded per-CLA value.
    pub max_bundle_size: Option<NonZeroU64>,
}

/// The BPA's acceptance verdict for a dispatched bundle — the return value
/// of [`Sink::dispatch`], and its only carrier.
///
/// Refusal is a normal protocol outcome, not an error: a CLA acts on it by
/// withholding its transfer acknowledgement, and carries on. Faults that
/// make the sink unusable travel as [`enum@Error`] instead.
#[must_use = "the verdict decides whether the transfer may be acknowledged"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    /// The BPA accepted the bundle: the CLA may acknowledge the transfer to
    /// its peer. Acceptance includes internal disposal — a bundle that is
    /// invalid, unwanted, or a duplicate is accepted and then dropped inside
    /// the BPA (with status reports where requested). Refusal would invite
    /// the previous node's custody/retransmission machinery to resend, and a
    /// resend of invalid content is byte-identical and can never succeed —
    /// so the first BPA to detect the problem accepts the bundle and
    /// terminates it.
    Accepted,
    /// The BPA refused acceptance: the CLA must not acknowledge the
    /// transfer, leaving its peer responsible for the bundle. Refusal is
    /// reserved for transfers the BPA could not take at all — a truncated
    /// stream (a resend may succeed), or a bundle over the BPA's size cap
    /// (the peer must stop offering it here).
    Refused,
}

/// The primary trait for a Convergence Layer Adapter (CLA).
///
/// A CLA is responsible for adapting the Bundle Protocol to a specific underlying
/// transport, such as TCP, UDP, or a custom link-layer protocol. It handles the
/// transmission and reception of bundles over its specific medium.
///
/// CLAs are often wrapped by a [`FlowControllerFactory`](crate::policy::FlowControllerFactory)
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
///     async fn on_register(&self, sink: Box<dyn Sink>, node_ids: &[NodeId], max_bundle_size: Option<NonZeroU64>) {
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
    /// `node_ids` are the BPA's own node identifiers.
    ///
    /// `max_bundle_size` is the *effective* size cap: the minimum of the
    /// BPA's configured cap, which the BPA enforces at dispatch, and the
    /// limit this CLA declared at registration. `None` means no cap was
    /// negotiated — neither side declared one, or a remote proxy could not
    /// learn the far BPA's cap. The CLA should propagate a `Some` cap into
    /// its own transfer limits, such as an advertised transfer MRU, so an
    /// over-cap transfer is never offered: rejection is deterministic, so a
    /// peer that keeps offering the same bundle loops forever.
    async fn on_register(
        &self,
        sink: Box<dyn Sink>,
        node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    );

    /// Called when the CLA is being unregistered.
    ///
    /// This is called in two scenarios:
    /// 1. The CLA dropped its Sink (CLA-initiated disconnection)
    /// 2. The BPA is shutting down (BPA-initiated disconnection)
    ///
    /// The CLA should perform cleanup: close connections, stop background tasks,
    /// and release resources. After this returns, the Sink is no longer functional.
    async fn on_unregister(&self);

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
    /// CLA answering [`ForwardOutcome::Accepted`] echoes it back in
    /// [`Sink::transfer_outcome`]. It is the correlation key, not data to
    /// transmit — CLAs that answer terminally may ignore it, and no CLA needs
    /// to parse the bundle to learn it.
    ///
    /// An implementation that needs the whole bundle in memory buffers the
    /// stream with [`buffer_stream`](crate::stream::buffer_stream), whose
    /// errors convert into this module's [`enum@Error`] via `?`.
    async fn forward(
        &self,
        lane: Option<u32>,
        cla_addr: &Address,
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<ForwardOutcome>;
}
