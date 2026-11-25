use core::fmt::{self, Display, Formatter};

// `eid::Service` is a service number, not the service trait defined here,
// so it is aliased to the word the rest of the code uses for it.
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, Service as ServiceId},
    status_report::ReasonCode,
};
use thiserror::Error;
use time::OffsetDateTime;

use crate::{
    Arc, async_trait,
    stream::{BufferError, Receiver},
};

mod factory;
mod sink;

pub mod raw;

pub(crate) mod table;

pub use crate::stream::Segment;
pub use factory::{BoxFactory, Context, Factory, factory_fn};
pub use sink::{SendOptions, Sink};

/// A `Result` whose error type is this module's [`enum@Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// An error raised by service registration or by sending a bundle.
#[derive(Debug, Error)]
pub enum Error {
    /// The requested service id is already registered by another service.
    #[error("There is already a service registered as {0}")]
    ServiceIdInUse(String),

    /// The provided DTN service name is syntactically invalid.
    #[error("Invalid dtn service name {0}")]
    DtnInvalidServiceName(String),

    /// The service id denotes the node's administrative endpoint, which is
    /// structurally registered and cannot be claimed by a service
    /// (RFC 9758 Section 5.7).
    #[error("Service id {0} identifies the administrative endpoint (RFC 9758 Section 5.7)")]
    AdministrativeEndpoint(String),

    /// No IPN node ID is configured on this BPA, so IPN services cannot register.
    #[error("There is no ipn node id configured")]
    NoIpnNodeId,

    /// No DTN node ID is configured on this BPA, so DTN services cannot register.
    #[error("There is no dtn node id configured")]
    NoDtnNodeId,

    /// The sink has been dropped or the BPA has shut down.
    #[error("The sink is disconnected")]
    Disconnected,

    /// The payload exceeds a size limit, which `max` reports. The limit is
    /// one of:
    ///
    /// * the BPA's `max_bundle_size`, checked against a declared wire size
    ///   or `total_len` before any payload byte is pulled, and against the
    ///   bytes a drain has taken;
    /// * a send's declared `total_len`, when its stream delivers more;
    /// * a transport's own limit, such as a gRPC message size, checked
    ///   before the oversized message breaks the underlying stream.
    #[error("Payload too large: {size} bytes exceeds the maximum of {max} bytes")]
    PayloadTooLarge { size: u64, max: u64 },

    /// A bundle stream completed with fewer bytes than its declared
    /// `total_len`. An implementation may size buffers, or frame a
    /// transfer, from that length before pulling the first segment, so a
    /// producer that under-delivers is rejected.
    #[error("Bundle stream delivered {size} bytes of the {expected} declared")]
    PayloadUnderrun { size: u64, expected: u64 },

    /// A stream declared a `total_len` that cannot be addressed in memory
    /// on this target. No buffer can hold it, so the stream is rejected
    /// before its first segment is pulled.
    #[error("declared length of {total_len} bytes is unaddressable on this target")]
    PayloadUnaddressable { total_len: u64 },

    /// The node id configuration does not support the requested service
    /// scheme.
    #[error(transparent)]
    NodeId(#[from] crate::node_ids::Error),

    /// The bundle's destination EID is not valid for sending.
    #[error("Invalid bundle destination {0}")]
    InvalidDestination(Eid),

    /// The bundle is a fragment. A source never emits fragments, since
    /// fragmentation is a forwarding-time action (RFC 9171 Section 5.8), so
    /// a service-built fragment is rejected.
    #[error("Cannot originate a bundle fragment")]
    FragmentedBundle,

    /// The bundle stream was cancelled: the producer dropped its sender
    /// before delivering the final segment, so no complete bundle arrived.
    #[error("The bundle stream was cancelled before completion")]
    StreamCancelled,

    /// The bundle was dropped before it was stored, with an optional reason
    /// code. The drop comes from an Originate filter, from an origination
    /// check such as an expired lifetime, an exhausted hop limit or a
    /// failed RFC 9171 validity check, or from an explicit Drop route.
    #[error("Bundle dropped{}", DroppedReason(.0))]
    Dropped(Option<ReasonCode>),

    /// A bundle with the same identity already exists in storage.
    #[error("Duplicate bundle already exists")]
    DuplicateBundle,

    /// The bundle failed BPv7 validation during parsing or construction.
    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

// Formats the tail of the `Dropped` message: the reason code if the drop
// carried one, and nothing otherwise.
struct DroppedReason<'a>(&'a Option<ReasonCode>);

impl Display for DroppedReason<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(reason) => write!(f, " ({reason:?})"),
            None => Ok(()),
        }
    }
}

/// The kind of bundle status event reported to a service.
///
/// The variants are the four status assertions of RFC 9171 Section
/// 6.1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusNotify {
    /// The bundle was received by the reporting node.
    Received,
    /// The bundle was forwarded by the reporting node.
    Forwarded,
    /// The bundle was delivered to its destination endpoint.
    Delivered,
    /// The bundle was deleted by the reporting node.
    Deleted,
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

/// A service that works with bundle payloads alone.
///
/// The BPA passes a delivered bundle's payload to the service and wraps
/// sent data in a bundle of its own, so bundle structure never appears
/// here. This is the contract most services want; a service that must
/// read or build the bundle itself implements [`raw::Service`].
///
/// The BPA builds the service from a [`Factory`], owns it, and drives its
/// lifecycle: [`register`](Self::register) brings it up,
/// [`deliver`](Self::deliver) passes it bundles addressed to its endpoint,
/// and [`unregister`](Self::unregister) takes it down again. Sent payloads
/// travel the other way, through the [`Sink`] in the [`Context`] the
/// service was built from.
#[async_trait]
pub trait Service: Send + Sync {
    /// Brings the service up, spawning tasks and opening the resources it
    /// serves from.
    ///
    /// Called once, when the registration opens. For a service declared on
    /// the [`BpaBuilder`](crate::builder::BpaBuilder) that is during
    /// [`Bpa::start`](crate::bpa::Bpa::start), after storage recovery, so a
    /// delivery never races the consistency check.
    ///
    /// The default implementation does nothing, which suits a service that
    /// is live as soon as it is built.
    ///
    /// # Errors
    ///
    /// An error means the service cannot run at all. The registration is
    /// undone and the error is returned to the caller that asked for it.
    async fn register(&self) -> Result<()> {
        Ok(())
    }

    /// Takes the service down, stopping background tasks and releasing
    /// resources.
    ///
    /// Called once, when the service unregisters or the BPA shuts down.
    /// The sink returns [`Error::Disconnected`] from every method by then,
    /// so nothing can be sent from here.
    ///
    /// The default implementation does nothing.
    ///
    /// # Errors
    ///
    /// An error is logged, and the teardown continues regardless.
    async fn unregister(&self) -> Result<()> {
        Ok(())
    }

    /// Delivers a bundle payload to this service, reading it from `stream`
    /// as a sequence of segments.
    ///
    /// The implementation pulls [`Segment::Next`] items from `stream` until
    /// a [`Segment::Final`], which may itself carry no bytes, completes the
    /// delivery. An implementation that needs the whole payload in memory
    /// reads it with [`buffer_stream`](crate::stream::buffer_stream), whose
    /// errors convert into this module's [`enum@Error`].
    ///
    /// `bundle_id.source` is the endpoint the payload came from.
    ///
    /// `ack_requested` is the source's request for an application-level
    /// acknowledgement (RFC 9171 Section 4.2.3). A service that honours it
    /// replies through its [`Sink`].
    ///
    /// `total_len` is the exact number of payload bytes `stream` delivers.
    /// An implementation may size buffers from it before pulling the first
    /// segment.
    ///
    /// # Errors
    ///
    /// A pull that returns `Err(`[`RecvError`](crate::stream::RecvError)`)`
    /// before the final segment means the producer aborted the delivery.
    /// The implementation must then neither act on the partial payload nor
    /// return `Ok`. An error parks the bundle as `WaitingForService`, and a
    /// later registration on the same EID delivers it again.
    async fn deliver(
        &self,
        bundle_id: &Id,
        expiry: OffsetDateTime,
        ack_requested: bool,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<()>;

    /// Passes on a status report received for a bundle this service
    /// sent.
    async fn report_status(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    );
}

/// Something a service can be registered with: a local BPA, or a proxy
/// for a remote one.
///
/// This is the dyn-compatible trait, taking an erased [`BoxFactory`].
/// Callers use [`RegistrarExt`], which preserves the concrete component
/// type.
///
/// `id` is the endpoint's service number, and `None` asks the BPA to
/// allocate one. The embedder owns that namespace, so two instances of the
/// same service crate are distinguished here rather than by the crate.
///
/// An implementation returns `Ok` only once it has both built the factory
/// and started the service.
#[async_trait]
pub trait Registrar: Send + Sync {
    /// Builds the payload-only service from `factory`, registers it at
    /// `id`, and starts it.
    async fn register_service_boxed(
        &self,
        id: Option<ServiceId>,
        factory: BoxFactory<'_>,
    ) -> Result<()>;

    /// Builds the whole-bundle service from `factory`, registers it at
    /// `id`, and starts it.
    async fn register_raw_service_boxed(
        &self,
        id: Option<ServiceId>,
        factory: raw::BoxFactory<'_>,
    ) -> Result<()>;
}

/// The typed counterpart of [`Registrar`], implemented for every
/// registrar.
#[async_trait]
pub trait RegistrarExt: Registrar {
    /// Builds the payload-only service from `factory`, registers it at
    /// `id`, starts it, and returns the component.
    ///
    /// The returned handle is for the embedder's own use, such as reading
    /// the service's endpoint or driving it. The registrar owns the service
    /// until it is unregistered, so holding the handle does not keep the
    /// service registered, and dropping it does not unregister it.
    async fn register_service<F: Factory>(
        &self,
        id: Option<ServiceId>,
        factory: F,
    ) -> Result<Arc<F::Component>>;

    /// Builds the whole-bundle service from `factory`, registers it at
    /// `id`, starts it, and returns the component.
    async fn register_raw_service<F: raw::Factory>(
        &self,
        id: Option<ServiceId>,
        factory: F,
    ) -> Result<Arc<F::Component>>;
}

#[async_trait]
impl<T: Registrar + ?Sized> RegistrarExt for T {
    async fn register_service<F: Factory>(
        &self,
        id: Option<ServiceId>,
        factory: F,
    ) -> Result<Arc<F::Component>> {
        let mut component = None;
        self.register_service_boxed(id, BoxFactory::capturing(factory, &mut component))
            .await?;

        // `Registrar` returns `Ok` only after building the factory, and
        // building it fills the slot.
        Ok(component.expect("Registrar reported success without building the factory"))
    }

    async fn register_raw_service<F: raw::Factory>(
        &self,
        id: Option<ServiceId>,
        factory: F,
    ) -> Result<Arc<F::Component>> {
        let mut component = None;
        self.register_raw_service_boxed(id, raw::BoxFactory::capturing(factory, &mut component))
            .await?;

        Ok(component.expect("Registrar reported success without building the factory"))
    }
}
