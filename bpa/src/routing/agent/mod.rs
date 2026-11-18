//! The routing agents of one BPA: the contract they implement and the
//! table that owns them.
//!
//! An embedder implements [`Agent`] and hands the BPA a [`Factory`] for
//! it. The BPA builds the agent from a [`Context`] carrying the [`Sink`]
//! it pushes routes through, and `table::Table` owns it from there, until
//! the agent detaches or the BPA shuts down.

use crate::async_trait;

use super::Result;

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
/// routing information base through the [`Sink`] its [`Context`] carries:
/// static configuration, a link-state protocol and neighbour discovery
/// are all agents.
///
/// Agents are purely push-based. The BPA never calls back to ask for
/// work, unlike a [`Cla`](crate::cla::Cla), which is asked to forward, so
/// the trait carries nothing but the lifecycle the BPA drives: the BPA
/// builds the agent from a [`Factory`], owns it, brings it up with
/// [`start`](Self::start) and takes it down with
/// [`shutdown`](Self::shutdown), withdrawing every route the agent added.
#[async_trait]
pub trait Agent: Send + Sync {
    /// Brings the agent up: spawns tasks, installs its initial routes,
    /// opens the sources it learns routes from.
    ///
    /// Called once, after the BPA's storage recovery. An `Err` means the
    /// agent cannot run at all: the BPA detaches it and carries on.
    ///
    /// The default implementation does nothing, for an agent that is live
    /// from the moment it is built.
    async fn start(&self) -> Result<()> {
        Ok(())
    }

    /// Takes the agent down: stops background tasks, releases resources.
    ///
    /// Called once, when the agent detaches or the BPA shuts down. The
    /// sink answers [`Error::Disconnected`] to everything by then, and the
    /// agent's routes are withdrawn once this returns; an `Err` is logged
    /// and teardown continues regardless.
    ///
    /// The default implementation does nothing.
    ///
    /// [`Error::Disconnected`]: crate::routing::Error::Disconnected
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
