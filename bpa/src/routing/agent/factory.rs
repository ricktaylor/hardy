//! Constructing a routing agent.

use core::fmt::{self, Debug, Formatter};

use super::{Agent, Sink};
use crate::{Arc, node_ids::NodeIds, routing::Result};

/// Everything the BPA hands a routing agent at construction.
#[non_exhaustive]
pub struct Context {
    /// The channel back to the BPA, for the agent's whole active
    /// lifetime.
    pub sink: Arc<dyn Sink>,

    /// The BPA's own identities, the routes an agent installs are
    /// relative to.
    pub node_ids: Arc<NodeIds>,
}

impl Context {
    /// Mints a context. The BPA does this; an agent receives one.
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

/// Builds a routing agent against the node the BPA runs as.
///
/// An agent crate exposes its own factory, such as `Tvr::factory(&config)`,
/// so what an agent needs stays with the agent that knows it. One with
/// nothing to carry is written with [`factory_fn`].
pub trait Factory: Send {
    /// The agent this factory builds.
    type Component: Agent + 'static;

    /// Builds the agent from its context.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of `cx.sink`.
    /// Reading a routes file belongs in [`Agent::register`], which the
    /// BPA calls as soon as this returns.
    ///
    /// `Err` means the node the BPA runs as is one this agent cannot
    /// route for.
    fn build(self, cx: Context) -> Result<Arc<Self::Component>>;
}

/// A [`Factory`] from a closure, for an agent whose construction needs
/// nothing but its context.
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

/// A [`Factory`] erased for [`Registrar`](crate::routing::Registrar), which is
/// dyn-compatible and so cannot carry one.
///
/// A factory's one capability needs no dispatch once the associated type
/// is gone: the constructor becomes a closure.
/// [`RegistrarExt::register_routing_agent`] erases for the caller, so
/// this type is only named by a `Registrar` implementation.
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

    // Erases `factory`, leaving the agent it builds in `slot` so
    // `RegistrarExt::register_routing_agent` can hand the concrete type
    // back without a downcast.
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
    pub fn build(self, cx: Context) -> Result<Arc<dyn Agent>> {
        (self.0)(cx)
    }
}

impl Debug for BoxFactory<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxFactory").finish_non_exhaustive()
    }
}
