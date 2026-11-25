use core::fmt::{self, Display, Formatter};

// `eid::Service` is the service number inside an EID, not the trait of the
// same name this module defines; it is aliased to the word the code
// already uses for it.
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

/// A specialized `Result` type for service operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during service registration and bundle sending.
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

    /// The payload exceeds a size limit; `max` is the limit exceeded:
    ///
    /// * the BPA's `max_bundle_size`, against a bundle's declared wire size
    ///   at either originate door or an ADU send's declared `total_len`,
    ///   before a payload byte is pulled, or against the bytes a drain has
    ///   taken;
    /// * an ADU send's declared `total_len`, when its stream delivers more;
    /// * a transport-backed sink's (e.g. gRPC) pre-flight limit, instead of
    ///   letting the oversized message break the underlying stream.
    ///
    /// The fields are wire-size-domain `u64`.
    #[error("Payload too large: {size} bytes exceeds the maximum of {max} bytes")]
    PayloadTooLarge { size: u64, max: u64 },

    /// A bundle stream completed with fewer bytes than its declared
    /// `total_len`. An implementation may size buffers — or frame a
    /// transfer — from the declared length before pulling the first
    /// segment, so an under-delivering producer is rejected at the seam.
    /// The fields are wire-size-domain `u64`.
    #[error("Bundle stream delivered {size} bytes of the {expected} declared")]
    PayloadUnderrun { size: u64, expected: u64 },

    /// A stream declared a `total_len` too large to address in memory on
    /// this target: no buffer can hold it, so the stream is rejected
    /// before the first segment is pulled.
    #[error("declared length of {total_len} bytes is unaddressable on this target")]
    PayloadUnaddressable { total_len: u64 },

    /// The node ID configuration doesn't support the requested service scheme.
    #[error(transparent)]
    NodeId(#[from] crate::node_ids::Error),

    /// The bundle's destination EID is not valid for sending.
    #[error("Invalid bundle destination {0}")]
    InvalidDestination(Eid),

    /// The bundle is a fragment. Fragmentation is a forwarding-time action
    /// (RFC 9171 §5.8) — a source never emits fragments — so the raw
    /// originate door rejects a service-built fragment outright.
    #[error("Cannot originate a bundle fragment")]
    FragmentedBundle,

    /// The bundle stream was cancelled: the producer dropped its sender
    /// before delivering the final segment, so no complete bundle arrived.
    #[error("The bundle stream was cancelled before completion")]
    StreamCancelled,

    /// The bundle was dropped before it was stored — by an Originate filter,
    /// an origination gate check (an expired lifetime, an exhausted hop
    /// limit, or a failed RFC 9171 validity check), or an explicit Drop
    /// route — with an optional reason code.
    #[error("Bundle dropped{}", DroppedReason(.0))]
    Dropped(Option<ReasonCode>),

    /// A bundle with the same identity already exists in storage.
    #[error("Duplicate bundle already exists")]
    DuplicateBundle,

    /// The bundle failed BPv7 validation during parsing or construction.
    #[error(transparent)]
    InvalidBundle(#[from] hardy_bpv7::Error),

    /// An internal error from an underlying subsystem.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

// The `Dropped` message's tail: the reason code when the drop gave one,
// nothing otherwise.
struct DroppedReason<'a>(&'a Option<ReasonCode>);

impl Display for DroppedReason<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(reason) => write!(f, " ({reason:?})"),
            None => Ok(()),
        }
    }
}

/// The kind of bundle status event being reported to a service.
///
/// These correspond to the four status assertions defined in RFC 9171 Section 6.1.1.
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
/// The BPA hands a delivered bundle's payload to the service and wraps
/// sent data in a bundle of its own, so bundle structure never surfaces
/// here. This is the contract most services want; one that must see or
/// build the bundle itself implements [`raw::Service`].
///
/// The BPA builds the service from a [`Factory`], owns it, and drives its
/// lifecycle: [`register`](Self::register) brings it up,
/// [`deliver`](Self::deliver) hands it bundles addressed to its endpoint,
/// and [`unregister`](Self::unregister) takes it down again.
/// Traffic in the other direction goes through the [`Sink`] the
/// [`Context`] carries, which the service keeps for as long as it is
/// registered.
#[async_trait]
pub trait Service: Send + Sync {
    /// Brings the service up: spawns tasks, opens the resources it serves
    /// from.
    ///
    /// Called once, as the registration opens: at
    /// [`Bpa::start`](crate::bpa::Bpa::start) for a declared service,
    /// after storage recovery, so a delivery never races the
    /// consistency check, and at the registration call for any other.
    /// An `Err` means the service cannot run at all: the registration
    /// is undone, and the error reaches whoever asked for it.
    ///
    /// The default implementation does nothing, for a service that is
    /// live from the moment it is built.
    async fn register(&self) -> Result<()> {
        Ok(())
    }

    /// Takes the service down: stops background tasks, releases
    /// resources.
    ///
    /// Called once, when the service unregisters or the BPA shuts down.
    /// The sink answers [`Error::Disconnected`] to everything by then, so
    /// nothing can be sent from here; an `Err` is logged and teardown
    /// continues regardless.
    ///
    /// The default implementation does nothing.
    async fn unregister(&self) -> Result<()> {
        Ok(())
    }

    /// Delivers a bundle payload to this service as a stream of segments.
    ///
    /// The implementation pulls [`Segment::Next`] items from `stream`
    /// until [`Segment::Final`] (which may carry empty bytes) completes
    /// the delivery. The source endpoint of the payload is
    /// `bundle_id.source`.
    ///
    /// A pull returning `Err(`[`RecvError`](crate::stream::RecvError)`)`
    /// before `Final` means the producer aborted the delivery: the
    /// implementation must not act on the partial payload and must not
    /// return `Ok`. Returning `Err` parks the bundle as
    /// `WaitingForService`; a subsequent registration on the same EID
    /// re-delivers it.
    ///
    /// `ack_requested` carries the source's request for an
    /// application-level acknowledgement (RFC 9171 Section 4.2.3); a
    /// service that honours it answers through its [`Sink`].
    ///
    /// `total_len` is the exact number of payload bytes the stream will
    /// deliver — the sum of all segment payload lengths — carried as `u64`
    /// so it remains valid on 32-bit targets. An implementation may size
    /// buffers from it before pulling the first segment.
    ///
    /// An implementation that needs the whole payload in memory buffers the
    /// stream with [`buffer_stream`](crate::stream::buffer_stream), whose
    /// errors convert into this module's [`enum@Error`] via `?`.
    async fn deliver(
        &self,
        bundle_id: &Id,
        expiry: OffsetDateTime,
        ack_requested: bool,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<()>;

    /// Reports a status report received for a bundle this service sent.
    async fn report_status(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    );
}

/// Somewhere a service can be registered: a local BPA, or a proxy
/// standing in for a remote one.
///
/// This is the dyn-compatible half of the door, taking an erased
/// [`BoxFactory`]. Callers use [`RegistrarExt`] instead, which keeps the
/// concrete component type and hands it back.
///
/// `id` names the endpoint's service number, and `None` asks the BPA to
/// allocate one. The key comes first because the embedder owns the
/// namespace: two instances of the same service crate are told apart
/// here, without the crate's cooperation.
///
/// An `Ok` return means the factory was built and the service started;
/// the registrar may not report success without having done both.
#[async_trait]
pub trait Registrar: Send + Sync {
    /// Builds the payload-only service `factory` makes, registers it at
    /// `id`, and starts it.
    async fn register_service_boxed(
        &self,
        id: Option<ServiceId>,
        factory: BoxFactory<'_>,
    ) -> Result<()>;

    /// Builds the whole-bundle service `factory` makes, registers it at
    /// `id`, and starts it.
    async fn register_raw_service_boxed(
        &self,
        id: Option<ServiceId>,
        factory: raw::BoxFactory<'_>,
    ) -> Result<()>;
}

/// The typed [`Registrar`] door.
#[async_trait]
pub trait RegistrarExt: Registrar {
    /// Builds the payload-only service `factory` makes, registers it at
    /// `id`, starts it, and hands back the component the registrar now
    /// owns.
    ///
    /// The returned handle is for the embedder's own use: reading the
    /// service's endpoint, driving it, reading its statistics. Holding it
    /// does not keep the service registered, and dropping it does not
    /// unregister it: the registrar owns the service until something
    /// unregisters it.
    async fn register_service<F: Factory>(
        &self,
        id: Option<ServiceId>,
        factory: F,
    ) -> Result<Arc<F::Component>>;

    /// Builds the whole-bundle service `factory` makes, registers it at
    /// `id`, starts it, and hands back the component the registrar now
    /// owns.
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

        // `Registrar`'s contract: an `Ok` return means the factory was
        // built, and building it fills the slot.
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
