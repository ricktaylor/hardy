use hardy_bpv7::eid::Eid;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Arc, async_trait};

pub(crate) mod action;
pub(crate) mod agent;
pub(crate) mod rib;

pub use self::action::RouteAction;
pub use self::agent::{Agent, BoxFactory, Context, Factory, Sink, StaticAgent, factory_fn};

pub(crate) use self::rib::{Decision, DispatchAction, Rib, RibSnapshot, RoutingKey};

/// A specialized `Result` type for routing agent operations.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during routing agent operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Another agent is already attached under this name.
    #[error("A routing agent named {0} is already attached")]
    DuplicateName(String),

    /// An agent of this name is being torn down. The name is released once
    /// the teardown completes, so the attachment may be retried.
    #[error("The routing agent named {0} is detaching")]
    Detaching(String),

    /// The connection to the BPA has been lost.
    #[error("The sink is disconnected")]
    Disconnected,

    /// A route was rejected because the next-hop is the null endpoint.
    #[error("Route via null next-hop rejected")]
    NullNextHop,

    /// A route was rejected because the next-hop resolves to the local node.
    #[error("Route via own node {0} rejected")]
    ViaOwnNode(Eid),

    /// An internal error occurred.
    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

/// Somewhere a routing agent can be registered: a local BPA, or a proxy
/// standing in for a remote one.
///
/// This is the dyn-compatible half of the door, taking an erased
/// [`BoxFactory`]. Callers use [`RegistrarExt::register_routing_agent`]
/// instead, which keeps the concrete component type and hands it back.
///
/// An `Ok` return means the factory was built and the agent started; the
/// registrar may not report success without having done both.
#[async_trait]
pub trait Registrar: Send + Sync {
    /// Builds the agent `factory` makes, files it under `name`, and
    /// starts it.
    async fn register_routing_agent_boxed(&self, name: &str, factory: BoxFactory<'_>)
    -> Result<()>;
}

/// The typed [`Registrar`] door.
#[async_trait]
pub trait RegistrarExt: Registrar {
    /// Builds the agent `factory` makes, files it under `name`, starts
    /// it, and hands back the component the registrar now owns.
    ///
    /// The returned handle is for the embedder's own use: reconfiguring
    /// the agent, reading its statistics. Holding it does not keep the
    /// agent attached, and dropping it does not detach it: the registrar
    /// owns the agent until something detaches it by name.
    async fn register_routing_agent<F: Factory>(
        &self,
        name: &str,
        factory: F,
    ) -> Result<Arc<F::Component>>;
}

#[async_trait]
impl<T: Registrar + ?Sized> RegistrarExt for T {
    async fn register_routing_agent<F: Factory>(
        &self,
        name: &str,
        factory: F,
    ) -> Result<Arc<F::Component>> {
        let mut component = None;
        self.register_routing_agent_boxed(name, BoxFactory::capturing(factory, &mut component))
            .await?;

        // `Registrar`'s contract: an `Ok` return means the factory was
        // built, and building it fills the slot.
        Ok(component.expect("Registrar reported success without building the factory"))
    }
}

/// The identity of a routing table: the table half of a bundle's
/// `{table, key}` routing inputs, which a Classifier writes through
/// [`MetadataDelta::route_table`](crate::filter::slots::MetadataDelta::route_table).
///
/// Opaque while only the default table exists, so [`TableId::DEFAULT`] is
/// the one table an embedder can name. How tables are named, and who
/// creates them, is an open question of the tables design
/// (`docs/routing_table_redesign.md`, "Open questions"); this type is where
/// the answer lands.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct TableId(u32);

impl TableId {
    /// The default table: the one an unset `route_table` selects.
    pub const DEFAULT: Self = Self(0);

    /// A table by numeric id, for the crate's tests.
    #[cfg(test)]
    pub(crate) const fn new(id: u32) -> Self {
        Self(id)
    }
}
