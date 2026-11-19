//! Construction of a CLA, and the negotiation that precedes it.

use core::{
    fmt::{self, Debug, Formatter},
    num::{NonZeroU32, NonZeroU64},
};

use super::{AddressType, Cla, Result, Sink};
use crate::{Arc, node_ids::NodeIds};

/// A factory's preferences, read by the BPA before its CLA is built.
///
/// Each field is a preference, not a requirement: `None` leaves the BPA's
/// own value in force. The BPA reads the request once, before the CLA
/// exists, and answers with the [`Terms`] it settles on.
///
/// A factory sets the fields it cares about and defaults the rest:
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
    pub address_type: Option<AddressType>,

    /// The number of transfers the CLA can carry in flight at once.
    ///
    /// Lanes are parallel transport channels, such as QUIC streams, DSCP
    /// classes or separate TCP connections, with no priority among them:
    /// scheduling is the BPA egress policy's business, and the CLA
    /// transmits whatever arrives on a lane. A transfer on one lane must
    /// not head-of-line block another lane.
    ///
    /// `n` lanes are indexed `0..n`. `None` leaves the CLA unconstrained,
    /// as a datagram convergence layer is: every forward then arrives with
    /// lane `None`, each transfer on a new lane of an unlimited pool.
    pub lane_count: Option<NonZeroU32>,

    /// The CLA's own receive limit, in bytes.
    ///
    /// The BPA combines it with its own configured cap into
    /// [`Terms::max_bundle_size`]. It bounds reception only; the send-side
    /// limit is the peer's, negotiated per session, and never leaves the
    /// CLA.
    pub max_bundle_size: Option<NonZeroU64>,
}

/// The terms the BPA settled against a CLA's [`Request`].
///
/// The terms are supplied in the [`Context`] and are fixed for the life of
/// the registration. A CLA whose circumstances change unregisters and
/// registers again, which negotiates afresh.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Terms {
    /// The lanes the CLA transmits on, indexed `0..n`. `None` is an
    /// unlimited pool, where every transfer travels on a lane of its own.
    pub lane_count: Option<NonZeroU32>,

    /// The effective receive limit: the smaller of the BPA's configured
    /// cap and the CLA's own, or `None` if neither declared one.
    ///
    /// A CLA applies a `Some` limit to its own transfer limits, such as an
    /// advertised transfer MRU, so that a peer never offers an over-limit
    /// transfer: the BPA's refusal is deterministic, and a peer that keeps
    /// offering the same bundle never makes progress. Ingress enforces the
    /// BPA's configured cap rather than this combined value.
    pub max_bundle_size: Option<NonZeroU64>,
}

impl Terms {
    // The smaller receive limit wins, and a side that declared none takes
    // the other's. Lanes are the CLA's alone, so they pass through.
    pub(crate) fn negotiate(request: Request, max_bundle_size: Option<NonZeroU64>) -> Self {
        Self {
            lane_count: request.lane_count,
            max_bundle_size: match (max_bundle_size, request.max_bundle_size) {
                (Some(bpa), Some(cla)) => Some(bpa.min(cla)),
                (bpa, cla) => bpa.or(cla),
            },
        }
    }
}

/// The values the BPA supplies to a CLA at construction.
#[non_exhaustive]
pub struct Context {
    /// The CLA's interface to the BPA, usable until it unregisters.
    pub sink: Arc<dyn Sink>,

    pub node_ids: Arc<NodeIds>,

    pub terms: Terms,
}

impl Context {
    /// Creates a context. Called by the BPA, which supplies the context to
    /// [`Factory::build`].
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
/// A CLA crate exposes its own factory, such as
/// `Tcpclv4::factory(&config)`, so that a CLA's configuration stays in the
/// crate that defines it. A factory with nothing of its own to carry is
/// written with [`factory_fn`].
pub trait Factory: Send {
    /// The CLA this factory builds.
    type Component: Cla + 'static;

    /// The preferences this factory states, read once before
    /// [`build`](Self::build).
    fn request(&self) -> Request {
        Request::default()
    }

    /// Builds the CLA against the terms the BPA settled.
    ///
    /// This performs construction only: no I/O, no spawned tasks, and no
    /// use of `cx.sink`. Opening a listener belongs in [`Cla::register`],
    /// which the BPA calls as soon as this returns.
    ///
    /// # Errors
    ///
    /// An error means the CLA cannot work under the settled terms, which
    /// is the CLA's side of the negotiation.
    fn build(self, cx: Context) -> Result<Arc<Self::Component>>;
}

/// Creates a [`Factory`] from a request and a closure, for a CLA whose
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

/// A [`Factory`] with its component type erased, for
/// [`Registrar`](super::Registrar), which is dyn-compatible and so cannot
/// take a generic factory.
///
/// A factory's request is read at erasure and its constructor becomes a
/// closure, so neither needs dynamic dispatch.
/// [`RegistrarExt::register_cla`] erases the factory for the caller, so
/// only an implementation of `Registrar` names this type.
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

    // Erases `factory`, storing the CLA it builds in `slot` so that
    // `RegistrarExt::register_cla` can return the concrete type without a
    // downcast.
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

    /// The preferences the erased factory stated, read at erasure.
    pub fn request(&self) -> Request {
        self.request
    }

    /// Builds the CLA, as [`Factory::build`] does.
    ///
    /// # Errors
    ///
    /// Returns the error of the erased factory.
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
