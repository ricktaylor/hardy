//! Construction of a whole-bundle service.

use core::fmt::{self, Debug, Formatter};

use hardy_bpv7::eid::Eid;

use super::{Service, Sink};
use crate::{Arc, service::Result};

/// The values the BPA supplies to a whole-bundle service at
/// construction.
#[non_exhaustive]
pub struct Context {
    /// The service's interface to the BPA, usable until it unregisters.
    pub sink: Arc<dyn Sink>,

    /// The endpoint the service is registered at, and the source of
    /// every bundle it sends.
    ///
    /// A service number named by the embedder is resolved against the
    /// node's identities; where the embedder named none, the BPA
    /// allocates one.
    pub endpoint: Eid,
}

impl Context {
    /// Creates a context. Called by the BPA, which supplies the context
    /// to [`Factory::build`].
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

/// Builds a whole-bundle service at the endpoint the BPA settles on.
///
/// A service crate exposes its own factory, such as `Echo::factory()`, so
/// that a service's configuration stays in the crate that defines it. A
/// factory with nothing of its own to carry is written with
/// [`factory_fn`].
pub trait Factory: Send {
    /// The service this factory builds.
    type Component: Service + 'static;

    /// Builds the service at the endpoint the BPA settled.
    ///
    /// This performs construction only: no I/O, no spawned tasks, and no
    /// use of `cx.sink`. Opening a file belongs in
    /// [`Service::register`], which the BPA calls as soon as this
    /// returns.
    ///
    /// # Errors
    ///
    /// An error means the service cannot work at the settled endpoint.
    fn build(self, cx: Context) -> Result<Arc<Self::Component>>;
}

/// Creates a [`Factory`] from a closure, for a service whose
/// construction needs nothing but its context.
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

/// A [`Factory`] with its component type erased, for
/// [`Registrar`](crate::service::Registrar), which is dyn-compatible and
/// so cannot take a generic factory.
///
/// Erasure turns the factory's constructor into a closure, so it needs
/// no dynamic dispatch. [`RegistrarExt::register_raw_service`] erases the
/// factory for the caller, so only an implementation of `Registrar`
/// names this type.
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

    // Erases `factory`, storing the service it builds in `slot` so that
    // `RegistrarExt::register_raw_service` can return the concrete type
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
    ///
    /// # Errors
    ///
    /// Returns the error of the erased factory.
    pub fn build(self, cx: Context) -> Result<Arc<dyn Service>> {
        (self.0)(cx)
    }
}

impl Debug for BoxFactory<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxFactory").finish_non_exhaustive()
    }
}
