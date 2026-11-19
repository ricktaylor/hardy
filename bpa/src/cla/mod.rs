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

/// A `Result` whose error type is this module's [`enum@Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during CLA operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Another CLA is already registered under this name.
    #[error("A CLA named {0} is already registered")]
    DuplicateName(String),

    /// A CLA is still unregistering under this name. The name is released
    /// when that completes, and the registration can be retried.
    #[error("The CLA named {0} is unregistering")]
    Unregistering(String),

    /// The connection to the BPA has been lost.
    #[error("The sink is disconnected")]
    Disconnected,

    /// The bundle stream ended before its final segment. The producer went
    /// away mid-bundle, and the partial bytes are discarded.
    #[error("The bundle stream was cancelled before completion")]
    StreamCancelled,

    /// The bundle exceeds a size limit: the BPA's `max_bundle_size` at
    /// ingress, or a transport's own limit caught by a pre-flight check
    /// rather than by breaking the underlying stream.
    #[error("Bundle too large: {size} bytes exceeds the maximum of {max} bytes")]
    PayloadTooLarge { size: u64, max: u64 },

    /// A bundle stream completed with fewer bytes than its declared
    /// `total_len`. A transport may frame the wire transfer from that
    /// length before pulling the first segment, so a short delivery is
    /// rejected rather than shorting the transfer on the wire.
    #[error("Bundle stream delivered {size} bytes of the {expected} declared")]
    PayloadUnderrun { size: u64, expected: u64 },

    /// A stream declared a `total_len` that cannot be addressed in memory
    /// on this target. No buffer can hold it, so the stream is rejected
    /// before its first segment is pulled.
    #[error("declared length of {total_len} bytes is unaddressable on this target")]
    PayloadUnaddressable { total_len: u64 },

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
/// The variants encode who owns the transfer once `forward` returns, not
/// merely success or failure. An unreliable convergence layer is
/// fire-and-forget, and [`Sent`](Self::Sent) is all it ever knows.
/// [`Accepted`](Self::Accepted) is for reliable convergence layers that
/// learn their peer's acceptance later: returning before the
/// acknowledgement arrives frees the egress lane, so a session pipelines
/// transfers instead of serializing each lane on a wire round-trip.
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
/// Today the outcome only resolves the retained bundle, but it is the
/// peer-acceptance signal a reliability or custody-transfer layer would
/// consume, which is why reliable CLAs report it even though `Completed`
/// and a fire-and-forget [`Sent`](ForwardOutcome::Sent) currently resolve
/// identically.
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

/// The BPA's acceptance verdict for a received bundle, returned by
/// [`Sink::dispatch`].
///
/// Refusal is a normal protocol outcome, not an error: a CLA acts on it by
/// withholding its transfer acknowledgement, and carries on. Faults that
/// make the sink unusable travel as [`enum@Error`] instead.
#[must_use = "the verdict decides whether the transfer may be acknowledged"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    /// The BPA accepted the bundle: the CLA may acknowledge the transfer to
    /// its peer. This covers bundles the BPA then disposes of internally as
    /// invalid, unwanted or duplicate, with status reports where requested.
    /// Refusing those would invite the previous node to retransmit, and a
    /// resend of invalid content is byte-identical, so the first BPA to
    /// detect the problem accepts the bundle and terminates it.
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
/// [`register`](Self::register) opens the transport,
/// [`forward`](Self::forward) sends bundles, and
/// [`unregister`](Self::unregister) closes the transport again. Received
/// bundles and peer updates travel the other way, through the [`Sink`] in
/// the [`Context`] the CLA was built from.
///
/// An embedder may pair a CLA with a
/// [`FlowControllerFactory`](crate::policy::FlowControllerFactory) to rate
/// limit or prioritise the lanes the CLA declares.
#[async_trait]
pub trait Cla: Send + Sync {
    /// Brings the transport up, binding sockets, dialling peers and
    /// spawning listener tasks.
    ///
    /// Called once, when the registration opens. For a CLA declared on the
    /// [`BpaBuilder`](crate::builder::BpaBuilder) that is during
    /// [`Bpa::start`](crate::bpa::Bpa::start), after storage recovery, so a
    /// listener never races the consistency check.
    ///
    /// The default implementation does nothing, which suits a CLA that is
    /// live as soon as it is built.
    ///
    /// # Errors
    ///
    /// An error means the CLA cannot run at all. The registration is undone
    /// and the error is returned to the caller that asked for it. A CLA
    /// that is only temporarily unreachable returns `Ok` and retries on its
    /// own.
    async fn register(&self) -> Result<()> {
        Ok(())
    }

    /// Takes the transport down, closing connections, stopping background
    /// tasks and releasing resources.
    ///
    /// Called once, when the CLA unregisters or the BPA shuts down. The
    /// sink is already disconnected, so nothing can be received or reported
    /// from here.
    ///
    /// The default implementation does nothing.
    ///
    /// # Errors
    ///
    /// An error is logged, and the teardown continues regardless.
    async fn unregister(&self) -> Result<()> {
        Ok(())
    }

    /// Forwards a bundle to `cla_addr`, reading it from `stream` as a
    /// sequence of segments.
    ///
    /// The implementation pulls [`Segment::Next`] items from `stream` until
    /// a [`Segment::Final`], which may itself carry no bytes, completes the
    /// transfer. An implementation that needs the whole bundle in memory
    /// reads it with [`buffer_stream`](crate::stream::buffer_stream), whose
    /// errors convert into this module's [`enum@Error`].
    ///
    /// `total_len` is the exact number of bundle bytes `stream` delivers,
    /// for transports that announce a length up front, such as a TCPCLv4
    /// XFER_SEGMENT. An implementation may frame the wire transfer from it
    /// before pulling the first segment.
    ///
    /// `lane` is `None` for a transfer on a fresh lane of an unlimited
    /// pool, which is what a CLA declaring no lane count always receives,
    /// and `Some(i)` for a transfer on lane `i`. A CLA that declares a lane
    /// count but receives `None` picks a lane itself rather than fail the
    /// transfer.
    ///
    /// `bundle_id` correlates a deferred outcome: a CLA returning
    /// [`ForwardOutcome::Accepted`] passes it back to
    /// [`Sink::report_transfer`]. It is not data to transmit, and a CLA
    /// that answers terminally may ignore it.
    ///
    /// # Errors
    ///
    /// A pull that returns `Err(`[`RecvError`](crate::stream::RecvError)`)`
    /// before the final segment means the producer aborted the transfer.
    /// The CLA must then abandon the transfer rather than deliver a partial
    /// bundle, and must return an error: a truncation reported as success
    /// would leave the peer treating the delivery as complete.
    async fn forward(
        &self,
        lane: Option<u32>,
        cla_addr: &Address,
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<ForwardOutcome>;
}

/// Something a CLA can be registered with: a local BPA, or a proxy for a
/// remote one.
///
/// This is the dyn-compatible trait, taking an erased [`BoxFactory`].
/// Callers use [`RegistrarExt::register_cla`], which preserves the
/// concrete component type.
///
/// An implementation returns `Ok` only once it has both built the factory
/// and started the CLA.
#[async_trait]
pub trait Registrar: Send + Sync {
    /// Builds the CLA from `factory`, registers it under `name`, and starts
    /// it.
    async fn register_cla_boxed(&self, name: &str, factory: BoxFactory<'_>) -> Result<()>;
}

/// The typed counterpart of [`Registrar`], implemented for every
/// registrar.
#[async_trait]
pub trait RegistrarExt: Registrar {
    /// Builds the CLA from `factory`, registers it under `name`, starts it,
    /// and returns the component.
    ///
    /// The returned handle is for the embedder's own use, such as
    /// reconfiguring the CLA or reading its statistics. The registrar owns
    /// the CLA until it is unregistered through
    /// [`Sink::unregister`] or the BPA shuts
    /// down, so holding the handle does not keep the CLA registered, and
    /// dropping it does not unregister it.
    async fn register_cla<F: Factory>(&self, name: &str, factory: F) -> Result<Arc<F::Component>>;
}

#[async_trait]
impl<T: Registrar + ?Sized> RegistrarExt for T {
    async fn register_cla<F: Factory>(&self, name: &str, factory: F) -> Result<Arc<F::Component>> {
        let mut component = None;
        self.register_cla_boxed(name, BoxFactory::capturing(factory, &mut component))
            .await?;

        // `Registrar` returns `Ok` only after building the factory, and
        // building it fills the slot.
        Ok(component.expect("Registrar reported success without building the factory"))
    }
}
