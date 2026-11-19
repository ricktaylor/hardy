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

/// A `Result` whose error type is this module's [`enum@Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// An error raised by routing agent registration or by a route change.
#[derive(Debug, Error)]
pub enum Error {
    /// Another agent is already registered under this name.
    #[error("A routing agent named {0} is already registered")]
    DuplicateName(String),

    /// An agent of this name is still unregistering. The name is released
    /// once that completes, so the registration may be retried.
    #[error("The routing agent named {0} is unregistering")]
    Unregistering(String),

    /// The connection to the BPA has been lost.
    #[error("The sink is disconnected")]
    Disconnected,

    /// A route was rejected because the next-hop is the null endpoint.
    #[error("Route via null next-hop rejected")]
    NullNextHop,

    /// A route was rejected because the next-hop resolves to the local node.
    #[error("Route via own node {0} rejected")]
    ViaOwnNode(Eid),

    #[error(transparent)]
    Internal(#[from] Box<dyn core::error::Error + Send + Sync>),
}

/// Something a routing agent can be registered with: a local BPA, or a
/// proxy for a remote one.
///
/// This is the dyn-compatible trait, taking an erased [`BoxFactory`].
/// Callers use [`RegistrarExt::register_routing_agent`], which preserves
/// the concrete component type.
///
/// An implementation returns `Ok` only once it has both built the factory
/// and started the agent.
#[async_trait]
pub trait Registrar: Send + Sync {
    /// Builds the agent from `factory`, registers it under `name`, and
    /// starts it.
    async fn register_routing_agent_boxed(&self, name: &str, factory: BoxFactory<'_>)
    -> Result<()>;
}

/// The typed counterpart of [`Registrar`], implemented for every
/// registrar.
#[async_trait]
pub trait RegistrarExt: Registrar {
    /// Builds the agent from `factory`, registers it under `name`, starts
    /// it, and returns the component.
    ///
    /// The returned handle is for the embedder's own use, such as
    /// reconfiguring the agent or reading its statistics. The registrar
    /// owns the agent until it is unregistered through
    /// [`Sink::unregister`] or the BPA
    /// shuts down, so holding the handle does not keep the agent
    /// registered, and dropping it does not unregister it.
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

        // `Registrar` returns `Ok` only after building the factory, and
        // building it fills the slot.
        Ok(component.expect("Registrar reported success without building the factory"))
    }
}

/// The identity of a routing table, the table half of a bundle's
/// `{table, key}` routing inputs, which a Classifier writes through
/// [`MetadataDelta::route_table`](crate::filter::slots::MetadataDelta::route_table).
///
/// The type is opaque while only the default table exists, so
/// [`TableId::DEFAULT`] is the one table an embedder can name. How tables
/// are named, and who creates them, is an open question of the tables
/// design (`docs/routing_table_redesign.md`, "Open questions").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct TableId(u32);

impl TableId {
    /// The default table, selected by an unset `route_table`.
    pub const DEFAULT: Self = Self(0);

    /// Creates a table id from a numeric value, for the crate's tests.
    #[cfg(test)]
    pub(crate) const fn new(id: u32) -> Self {
        Self(id)
    }
}
