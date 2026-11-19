//! Routing agents: the trait an agent implements, and the table the BPA
//! holds them in.
//!
//! An embedder implements [`Agent`] and hands the BPA a [`Factory`] for
//! it. The BPA builds the agent from a [`Context`] carrying the [`Sink`]
//! the agent pushes routes through, and owns it until the agent
//! unregisters or the BPA shuts down.

use super::Result;
use crate::async_trait;

mod factory;
mod sink;
mod static_agent;

pub(crate) mod table;

pub use self::factory::{BoxFactory, Context, Factory, factory_fn};
pub use self::sink::Sink;
pub use self::static_agent::StaticAgent;

/// A component that supplies routes to the BPA.
///
/// An agent discovers or computes routes and pushes them into the BPA's
/// routing information base through the [`Sink`] in its [`Context`].
/// Static configuration, a link-state protocol and neighbour discovery
/// are all agents.
///
/// An agent is push-only. Unlike a [`Cla`](crate::cla::Cla), which is
/// asked to forward, an agent is never called back for work, so the trait
/// carries only the lifecycle the BPA drives: the BPA builds the agent
/// from a [`Factory`], owns it, brings it up with
/// [`register`](Self::register), and takes it down with
/// [`unregister`](Self::unregister), withdrawing every route it added.
#[async_trait]
pub trait Agent: Send + Sync {
    /// Brings the agent up, spawning tasks, installing its initial routes
    /// and opening the sources it learns routes from.
    ///
    /// Called once, when the registration opens. For an agent declared on
    /// the [`BpaBuilder`](crate::builder::BpaBuilder) that is during
    /// [`Bpa::start`](crate::bpa::Bpa::start), after storage recovery.
    ///
    /// The default implementation does nothing, which suits an agent that
    /// is live as soon as it is built.
    ///
    /// # Errors
    ///
    /// An error means the agent cannot run at all. The registration is
    /// undone and the error is returned to the caller that asked for it.
    async fn register(&self) -> Result<()> {
        Ok(())
    }

    /// Takes the agent down, stopping background tasks and releasing
    /// resources.
    ///
    /// Called once, when the agent unregisters or the BPA shuts down. The
    /// sink already answers [`Error::Disconnected`], and the agent's
    /// routes are withdrawn once this returns.
    ///
    /// The default implementation does nothing.
    ///
    /// # Errors
    ///
    /// An error is logged, and teardown continues.
    ///
    /// [`Error::Disconnected`]: crate::routing::Error::Disconnected
    async fn unregister(&self) -> Result<()> {
        Ok(())
    }
}
