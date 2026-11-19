//! Constructing a whole-bundle service.

use core::fmt::{self, Debug, Formatter};

use hardy_bpv7::eid::Eid;

use super::{Service, Sink};
use crate::{Arc, service::Result};

/// Everything the BPA hands a whole-bundle service at construction.
#[non_exhaustive]
pub struct Context {
    /// The channel back to the BPA, for the service's whole active
    /// lifetime.
    pub sink: Arc<dyn Sink>,

    /// The endpoint the service is registered at, and the source of every
    /// bundle it sends.
    ///
    /// The one value the BPA settles for a service: an embedder naming a
    /// service number gets that number resolved against the node's
    /// identities, and one naming none gets an allocated number.
    pub endpoint: Eid,
}

impl Context {
    /// Mints a context. The BPA does this; a service receives one.
    pub fn new(sink: Arc<dyn Sink>, endpoint: Eid) -> Self {
        Self { sink, endpoint }
    }
}

impl Debug for Context {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Builds a whole-bundle service against the endpoint the BPA settles on.
///
/// A service crate exposes its own factory, such as `Echo::factory()`, so
/// what a service needs stays with the service that knows it. One with
/// nothing to carry is written with [`factory_fn`].
pub trait Factory: Send {
    /// The service this factory builds.
    type Component: Service + 'static;

    /// Builds the service at the endpoint the BPA settled.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of `cx.sink`.
    /// Opening a file belongs in [`Service::register`], which the BPA
    /// calls as soon as this returns.
    ///
    /// `Err` means the settled endpoint is one this service cannot work
    /// at: the service's half of the negotiation.
    fn build(self, cx: Context) -> Result<Arc<Self::Component>>;
}

/// A [`Factory`] from a closure, for a service whose construction needs
/// nothing but its context.
///
/// ```ignore
/// raw::factory_fn(|cx| Ok(Arc::new(MyService::new(cx))))
/// ```
pub fn factory_fn<C, F>(build: F) -> impl Factory<Component = C>
where
    C: Service + 'static,
    F: FnOnce(Context) -> Result<Arc<C>> + Send,
{
    FactoryFn(build)
}

struct FactoryFn<F>(F);

impl<C, F> Factory for FactoryFn<F>
where
    C: Service + 'static,
    F: FnOnce(Context) -> Result<Arc<C>> + Send,
{
    type Component = C;

    fn build(self, cx: Context) -> Result<Arc<C>> {
        (self.0)(cx)
    }
}

/// A [`Factory`] erased for [`Registrar`](crate::service::Registrar),
/// which is dyn-compatible and so cannot carry one.
///
/// A factory's one capability needs no dispatch once the associated type
/// is gone: the constructor becomes a closure.
/// [`RegistrarExt::register_raw_service`] erases for the caller, so this
/// type is only named by a `Registrar` implementation.
///
/// [`RegistrarExt::register_raw_service`]: crate::service::RegistrarExt::register_raw_service
pub struct BoxFactory<'a>(Box<dyn FnOnce(Context) -> Result<Arc<dyn Service>> + Send + 'a>);

impl<'a> BoxFactory<'a> {
    /// Erases `factory`.
    pub fn new<F: Factory + 'a>(factory: F) -> Self {
        Self(Box::new(|cx| {
            factory.build(cx).map(|service| service as Arc<dyn Service>)
        }))
    }

    // Erases `factory`, leaving the service it builds in `slot` so
    // `RegistrarExt::register_raw_service` can hand the concrete type back
    // without a downcast.
    pub(crate) fn capturing<F: Factory + 'a>(
        factory: F,
        slot: &'a mut Option<Arc<F::Component>>,
    ) -> Self {
        Self(Box::new(|cx| {
            let service = factory.build(cx)?;
            *slot = Some(service.clone());
            Ok(service)
        }))
    }

    /// Builds the service, as [`Factory::build`] does.
    pub fn build(self, cx: Context) -> Result<Arc<dyn Service>> {
        (self.0)(cx)
    }
}

impl Debug for BoxFactory<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxFactory").finish_non_exhaustive()
    }
}
