//! Construction of a routing agent.

use core::fmt::{self, Debug, Formatter};

use super::{Agent, Sink};
use crate::{Arc, node_ids::NodeIds, routing::Result};

/// The values the BPA supplies to a routing agent at construction.
#[non_exhaustive]
pub struct Context {
    /// The agent's interface to the BPA, usable until it unregisters.
    pub sink: Arc<dyn Sink>,

    /// The identities an agent's routes are relative to.
    pub node_ids: Arc<NodeIds>,
}

impl Context {
    /// Creates a context. Called by the BPA, which supplies the context
    /// to [`Factory::build`].
    pub fn new(sink: Arc<dyn Sink>, node_ids: Arc<NodeIds>) -> Self {
        Self { sink, node_ids }
    }
}

impl Debug for Context {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("node_ids", &self.node_ids)
            .finish_non_exhaustive()
    }
}

/// Builds a routing agent for the node the BPA runs as.
///
/// An agent crate exposes its own factory, such as
/// `Tvr::factory(&config)`, so that an agent's configuration stays in the
/// crate that defines it. A factory with nothing of its own to carry is
/// written with [`factory_fn`].
pub trait Factory: Send {
    /// The agent this factory builds.
    type Component: Agent + 'static;

    /// Builds the agent from its context.
    ///
    /// This performs construction only: no I/O, no spawned tasks, and no
    /// use of `cx.sink`. Reading a routes file belongs in
    /// [`Agent::register`], which the BPA calls as soon as this returns.
    ///
    /// # Errors
    ///
    /// An error means the agent cannot route for the node the BPA runs
    /// as.
    fn build(self, cx: Context) -> Result<Arc<Self::Component>>;
}

/// Creates a [`Factory`] from a closure, for an agent whose construction
/// needs nothing but its context.
///
/// ```ignore
/// routing::factory_fn(|cx| Ok(Arc::new(MyAgent::new(cx))))
/// ```
pub fn factory_fn<C, F>(build: F) -> impl Factory<Component = C>
where
    C: Agent + 'static,
    F: FnOnce(Context) -> Result<Arc<C>> + Send,
{
    FactoryFn(build)
}

struct FactoryFn<F>(F);

impl<C, F> Factory for FactoryFn<F>
where
    C: Agent + 'static,
    F: FnOnce(Context) -> Result<Arc<C>> + Send,
{
    type Component = C;

    fn build(self, cx: Context) -> Result<Arc<C>> {
        (self.0)(cx)
    }
}

/// A [`Factory`] with its component type erased, for
/// [`Registrar`](crate::routing::Registrar), which is dyn-compatible and
/// so cannot take a generic factory.
///
/// Erasure turns the factory's constructor into a closure, so it needs no
/// dynamic dispatch. [`RegistrarExt::register_routing_agent`] erases the
/// factory for the caller, so only an implementation of `Registrar` names
/// this type.
///
/// [`RegistrarExt::register_routing_agent`]: crate::routing::RegistrarExt::register_routing_agent
pub struct BoxFactory<'a>(Box<dyn FnOnce(Context) -> Result<Arc<dyn Agent>> + Send + 'a>);

impl<'a> BoxFactory<'a> {
    /// Erases `factory`.
    pub fn new<F: Factory + 'a>(factory: F) -> Self {
        Self(Box::new(|cx| {
            factory.build(cx).map(|agent| agent as Arc<dyn Agent>)
        }))
    }

    // Erases `factory`, storing the agent it builds in `slot` so that
    // `RegistrarExt::register_routing_agent` can return the concrete type
    // without a downcast.
    pub(crate) fn capturing<F: Factory + 'a>(
        factory: F,
        slot: &'a mut Option<Arc<F::Component>>,
    ) -> Self {
        Self(Box::new(|cx| {
            let agent = factory.build(cx)?;
            *slot = Some(agent.clone());
            Ok(agent)
        }))
    }

    /// Builds the agent, as [`Factory::build`] does.
    ///
    /// # Errors
    ///
    /// Returns the error of the erased factory.
    pub fn build(self, cx: Context) -> Result<Arc<dyn Agent>> {
        (self.0)(cx)
    }
}

impl Debug for BoxFactory<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxFactory").finish_non_exhaustive()
    }
}
