//! Constructing a CLA, and the negotiation that precedes it.

use core::{
    fmt::{self, Debug, Formatter},
    num::{NonZeroU32, NonZeroU64},
};

use super::{AddressType, Cla, Result, Sink};
use crate::{Arc, node_ids::NodeIds};

/// What a CLA asks of the BPA.
///
/// Every field is a preference rather than a demand: `None` leaves the
/// BPA's own value governing. The BPA reads the request once, before the
/// CLA exists, and answers with the [`Terms`] it settles on.
///
/// A CLA names the fields it cares about and defaults the rest:
///
/// ```
/// # use hardy_bpa::cla::{AddressType, Request};
/// let request = Request {
///     address_type: Some(AddressType::Tcp),
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// The address type this CLA handles.
    pub address_type: Option<AddressType>,

    /// The CLA's lane count: its honest parallelism, the number of
    /// transfers it can usefully carry in flight at once.
    ///
    /// Lanes are parallel transport channels (QUIC streams, DSCP classes,
    /// separate TCP connections) carrying in-flight transfers with no
    /// explicit priority among them. Scheduling and prioritisation live in
    /// the BPA's egress policy, which decides what is forwarded onto each
    /// lane; the CLA simply transmits what arrives on a lane, and one
    /// lane's in-flight transfer must not head-of-line block another's.
    ///
    /// `n` lanes are indexed `0..n`. Left `None`, the CLA is effectively
    /// unconstrained, as a datagram convergence layer is, and every
    /// forward arrives with lane `None`, each transfer travelling on a new
    /// lane from an infinite pool.
    pub lane_count: Option<NonZeroU32>,

    /// The CLA's own receive limit, in bytes.
    ///
    /// The BPA folds it with its own configured cap into
    /// [`Terms::max_reception_size`]. It bounds reception on both sides;
    /// the send-side limit is the peer's, negotiated per session, and
    /// never leaves the CLA.
    pub max_reception_size: Option<NonZeroU64>,
}

/// What the BPA settled against a CLA's [`Request`].
///
/// Delivered in the [`Context`] and fixed for the life of the attachment:
/// a CLA whose circumstances change detaches and attaches again,
/// negotiating from scratch. A CLA's tests mint terms the way a CLA
/// declares a request, to build the CLA under the terms they exercise.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Terms {
    /// The lanes the CLA transmits on, indexed `0..n`; `None` is the
    /// infinite pool, where every transfer travels on a lane of its own.
    pub lane_count: Option<NonZeroU32>,

    /// The effective receive limit: the smaller of the BPA's configured
    /// cap and the CLA's own, or `None` where neither declared one.
    ///
    /// A CLA propagates a `Some` limit into its own transfer limits, such
    /// as an advertised transfer MRU, so an over-limit transfer is never
    /// offered: refusal is deterministic, and a peer that keeps offering
    /// the same bundle loops for ever. Ingress enforces the BPA's
    /// configured cap, not this folded value.
    pub max_reception_size: Option<NonZeroU64>,
}

impl Terms {
    // Settles a request against the BPA's own receive limit, the one
    // value both sides declare. The smaller limit wins, and a side that
    // declared none defers to the other. Lanes are the CLA's own count,
    // so they pass through as asked.
    pub(crate) fn negotiate(request: Request, max_reception_size: Option<NonZeroU64>) -> Self {
        Self {
            lane_count: request.lane_count,
            max_reception_size: match (max_reception_size, request.max_reception_size) {
                (Some(bpa), Some(cla)) => Some(bpa.min(cla)),
                (bpa, cla) => bpa.or(cla),
            },
        }
    }
}

/// Everything the BPA hands a CLA at construction.
#[non_exhaustive]
pub struct Context {
    /// The channel back to the BPA, for the CLA's whole active lifetime.
    pub sink: Arc<dyn Sink>,

    /// The BPA's own identities: a fact handed over, not a term agreed.
    pub node_ids: Arc<NodeIds>,

    /// What the BPA settled against the CLA's request.
    pub terms: Terms,
}

impl Context {
    /// Mints a context. The BPA does this; a CLA receives one.
    pub fn new(sink: Arc<dyn Sink>, node_ids: Arc<NodeIds>, terms: Terms) -> Self {
        Self {
            sink,
            node_ids,
            terms,
        }
    }
}

impl Debug for Context {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("node_ids", &self.node_ids)
            .field("terms", &self.terms)
            .finish_non_exhaustive()
    }
}

/// Builds a CLA against the terms the BPA settles on.
///
/// A CLA crate exposes its own factory, such as `Tcpclv4::factory(&config)`,
/// so what a CLA needs stays with the CLA that knows it. One with nothing
/// to ask for is written with [`factory_fn`].
pub trait Factory: Send {
    /// The CLA this factory builds.
    type Component: Cla + 'static;

    /// What this CLA needs from the BPA, read once, before construction.
    fn request(&self) -> Request {
        Request::default()
    }

    /// Builds the CLA against the terms the BPA settled.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of `cx.sink`. The
    /// CLA does not run until [`Cla::start`], so a constructor that opens
    /// a listener works at the runtime door and races storage recovery at
    /// the build-time one.
    ///
    /// `Err` means the settled terms are ones this CLA cannot work under:
    /// the CLA's half of the negotiation.
    fn build(self, cx: Context) -> Result<Arc<Self::Component>>;
}

/// A [`Factory`] from a request and a closure, for a CLA whose
/// construction needs nothing but its context.
///
/// ```ignore
/// cla::factory_fn(cla::Request::default(), |cx| Ok(Arc::new(MyCla::new(cx))))
/// ```
pub fn factory_fn<C, F>(request: Request, build: F) -> impl Factory<Component = C>
where
    C: Cla + 'static,
    F: FnOnce(Context) -> Result<Arc<C>> + Send,
{
    FactoryFn { request, build }
}

struct FactoryFn<F> {
    request: Request,
    build: F,
}

impl<C, F> Factory for FactoryFn<F>
where
    C: Cla + 'static,
    F: FnOnce(Context) -> Result<Arc<C>> + Send,
{
    type Component = C;

    fn request(&self) -> Request {
        self.request
    }

    fn build(self, cx: Context) -> Result<Arc<C>> {
        (self.build)(cx)
    }
}

/// A [`Factory`] erased for [`Registrar`](super::Registrar), which is
/// dyn-compatible and so cannot carry one.
///
/// A factory has two capabilities, and neither needs dispatch once the
/// associated type is gone: the request is answered at erasure, and the
/// constructor becomes a closure. [`RegistrarExt::register_cla`] erases
/// for the caller, so this type is only named by a `Registrar`
/// implementation.
///
/// [`RegistrarExt::register_cla`]: super::RegistrarExt::register_cla
pub struct BoxFactory<'a> {
    request: Request,
    build: Box<dyn FnOnce(Context) -> Result<Arc<dyn Cla>> + Send + 'a>,
}

impl<'a> BoxFactory<'a> {
    /// Erases `factory`.
    pub fn new<F: Factory + 'a>(factory: F) -> Self {
        Self {
            request: factory.request(),
            build: Box::new(|cx| factory.build(cx).map(|cla| cla as Arc<dyn Cla>)),
        }
    }

    // Erases `factory`, leaving the CLA it builds in `slot` so
    // `RegistrarExt::register_cla` can hand the concrete type back without
    // a downcast.
    pub(crate) fn capturing<F: Factory + 'a>(
        factory: F,
        slot: &'a mut Option<Arc<F::Component>>,
    ) -> Self {
        Self {
            request: factory.request(),
            build: Box::new(|cx| {
                let cla = factory.build(cx)?;
                *slot = Some(cla.clone());
                Ok(cla)
            }),
        }
    }

    /// What the erased factory asks of the BPA, read at erasure.
    pub fn request(&self) -> Request {
        self.request
    }

    /// Builds the CLA, as [`Factory::build`] does.
    pub fn build(self, cx: Context) -> Result<Arc<dyn Cla>> {
        (self.build)(cx)
    }
}

impl Debug for BoxFactory<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxFactory")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}
