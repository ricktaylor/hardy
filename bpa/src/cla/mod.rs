use hardy_bpv7::bundle::Id;
use thiserror::Error;

use crate::{
    Arc, async_trait,
    stream::{BufferError, Receiver},
};

mod address;
mod factory;
mod queue;
mod sink;

pub(crate) mod peers;
pub(crate) mod table;

pub use crate::stream::Segment;
pub use address::{Address, AddressType};
pub use factory::{BoxFactory, Context, Factory, Request, Terms, factory_fn};
pub use sink::Sink;

/// A specialized `Result` type for CLA operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during CLA operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Another CLA is already attached under this name.
    #[error("A CLA named {0} is already attached")]
    DuplicateName(String),

    /// A CLA of this name is being torn down. The name is released once
    /// the teardown completes, so the attachment may be retried.
    #[error("The CLA named {0} is detaching")]
    Detaching(String),

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
/// ever knows. [`Accepted`](Self::Accepted) exists only for *reliable*
/// convergence layers that learn their peer's acceptance later: returning
/// early frees the egress lane while that acknowledgement is in flight, so
/// a session can pipeline many transfers concurrently. This deferred
/// callback is the pipelining seam: a `forward` that awaited the final
/// outcome would serialize every lane on a wire round-trip. Local delivery
/// has no analogue of `Accepted` because `on_deliver` completes in-process,
/// with no round-trip to wait out.
pub enum ForwardOutcome {
    /// The bundle was sent, and no further outcome follows: the terminal,
    /// fire-and-forget answer of an unreliable convergence layer.
    Sent,
    /// The CLA has taken ownership of the bundle and reports the transfer's
    /// outcome later via [`Sink::report_transfer`]. The BPA retains the
    /// bundle until the outcome arrives or the peer is removed; a lifetime
    /// that expires mid-transfer is enforced when the outcome arrives, not
    /// by recalling the transfer.
    Accepted,
    /// The bundle could not be sent because the neighbor is no longer available.
    NoNeighbour,
}

/// The final outcome of a transfer previously answered
/// [`ForwardOutcome::Accepted`].
///
/// Today the outcome only resolves the retained bundle, but it is
/// deliberately the peer-acceptance signal a future reliability /
/// custody-transfer layer consumes, which is why reliable CLAs report it
/// even though `Completed` and a fire-and-forget
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

/// The BPA's acceptance verdict for a received bundle: the return value
/// of [`Sink::dispatch`], and its only carrier.
///
/// Refusal is a normal protocol outcome, not an error: a CLA acts on it by
/// withholding its transfer acknowledgement, and carries on. Faults that
/// make the sink unusable travel as [`enum@Error`] instead.
#[must_use = "the verdict decides whether the transfer may be acknowledged"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    /// The BPA accepted the bundle: the CLA may acknowledge the transfer to
    /// its peer. Acceptance includes internal disposal: a bundle that is
    /// invalid, unwanted, or a duplicate is accepted and then dropped inside
    /// the BPA (with status reports where requested). Refusal would invite
    /// the previous node's custody/retransmission machinery to resend, and a
    /// resend of invalid content is byte-identical and can never succeed,
    /// so the first BPA to detect the problem accepts the bundle and
    /// terminates it.
    Accepted,
    /// The BPA refused acceptance: the CLA must not acknowledge the
    /// transfer, leaving its peer responsible for the bundle. Refusal is
    /// reserved for transfers the BPA could not take at all: a truncated
    /// stream (a resend may succeed), or a bundle over the BPA's size cap
    /// (the peer must stop offering it here).
    Refused,
}

/// A Convergence Layer Adapter: the Bundle Protocol carried over one
/// underlying transport, such as TCP, UDP, or a custom link layer.
///
/// A CLA transmits and receives bundles over its own medium. The BPA
/// builds it from a [`Factory`], owns it, and drives its lifecycle:
/// [`start`](Self::start) opens the transport, [`forward`](Self::forward)
/// carries bundles out, and [`shutdown`](Self::shutdown) closes everything
/// again. Traffic in the other direction goes through the [`Sink`] the
/// [`Context`] carries, which the CLA keeps for as long as it is attached.
///
/// A CLA is commonly paired with a
/// [`FlowControllerFactory`](crate::policy::FlowControllerFactory), which
/// adds rate limiting or prioritisation over the lanes the CLA declares.
#[async_trait]
pub trait Cla: Send + Sync {
    /// Brings the transport up: binds sockets, dials peers, spawns
    /// listener tasks.
    ///
    /// Called once, after the BPA's storage recovery, so a listener never
    /// races the consistency check. An `Err` means the CLA cannot run at
    /// all: the BPA detaches it and carries on degraded, reporting the
    /// failure from [`Bpa::start`](crate::bpa::Bpa::start). A CLA that is
    /// merely unreachable for now returns `Ok` and retries on its own.
    ///
    /// The default implementation does nothing, for a CLA that is live
    /// from the moment it is built.
    async fn start(&self) -> Result<()> {
        Ok(())
    }

    /// Takes the transport down: closes connections, stops background
    /// tasks, releases resources.
    ///
    /// Called once, when the CLA detaches or the BPA shuts down. The sink
    /// is already dead by then, so nothing can be received or reported
    /// from here; an `Err` is logged and teardown continues regardless.
    ///
    /// The default implementation does nothing.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Forwards a bundle, delivered as a stream of segments, to a specific CLA
    /// address over a given lane.
    ///
    /// Lane `None` requests a new lane from the infinite pool, the
    /// default for CLAs that declare no lane limit, where every transfer
    /// travels on its own fresh lane (a datagram CL). `Some(i)` directs
    /// the transfer onto explicit lane `i`. As a safety net, a CLA that
    /// declares explicit lanes but still receives `None` should pick a
    /// lane arbitrarily (e.g. at random) rather than fail the transfer.
    /// Mirroring [`Sink::dispatch`] on the egress side, the implementation
    /// pulls [`Segment::Next`] items from `stream` until
    /// [`Segment::Final`] (which may carry empty bytes) completes the
    /// transfer.
    ///
    /// A pull returning `Err(`[`RecvError`](crate::stream::RecvError)`)`
    /// before `Final` means the producer aborted the transfer: the CLA must
    /// tear down without delivering a partial bundle to the peer, and must
    /// not return `Ok`: a truncated transfer surfaced as success would let
    /// the peer treat delivery as complete.
    ///
    /// `total_len` is the exact number of bundle bytes the stream delivers,
    /// the sum of all segment payload lengths. It is a framing hint for
    /// transports that must announce a length up front (e.g. a TCPCLv4
    /// XFER_SEGMENT length), carried as `u64` so it remains valid on
    /// 32-bit targets. Producers must be exact: an implementation may frame
    /// the wire transfer from it before pulling the first segment.
    ///
    /// `bundle_id` identifies the transfer if the CLA defers its outcome: a
    /// CLA answering [`ForwardOutcome::Accepted`] echoes it back in
    /// [`Sink::report_transfer`]. It is the correlation key, not data to
    /// transmit; CLAs that answer terminally may ignore it, and no CLA needs
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

/// Somewhere a CLA can be registered: a local BPA, or a proxy standing in
/// for a remote one.
///
/// This is the dyn-compatible half of the door, taking an erased
/// [`BoxFactory`]. Callers use [`RegistrarExt::register_cla`] instead,
/// which keeps the concrete component type and hands it back.
///
/// An `Ok` return means the factory was built and the CLA started; the
/// registrar may not report success without having done both.
#[async_trait]
pub trait Registrar: Send + Sync {
    /// Builds the CLA `factory` makes, files it under `name`, and starts
    /// it.
    async fn register_cla_boxed(&self, name: &str, factory: BoxFactory<'_>) -> Result<()>;
}

/// The typed [`Registrar`] door.
#[async_trait]
pub trait RegistrarExt: Registrar {
    /// Builds the CLA `factory` makes, files it under `name`, starts it,
    /// and hands back the component the registrar now owns.
    ///
    /// The returned handle is for the embedder's own use: reconfiguring
    /// the CLA, reading its statistics. Holding it does not keep the CLA
    /// attached, and dropping it does not detach it: the registrar owns
    /// the CLA until something detaches it by name.
    async fn register_cla<F: Factory>(&self, name: &str, factory: F) -> Result<Arc<F::Component>>;
}

#[async_trait]
impl<T: Registrar + ?Sized> RegistrarExt for T {
    async fn register_cla<F: Factory>(&self, name: &str, factory: F) -> Result<Arc<F::Component>> {
        let mut component = None;
        self.register_cla_boxed(name, BoxFactory::capturing(factory, &mut component))
            .await?;

        // `Registrar`'s contract: an `Ok` return means the factory was
        // built, and building it fills the slot.
        Ok(component.expect("Registrar reported success without building the factory"))
    }
}
