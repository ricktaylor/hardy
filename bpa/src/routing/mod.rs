pub mod agent;

pub(crate) mod action;
pub(crate) mod rib;
pub(crate) mod table;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub use self::action::RouteAction;
pub use self::agent::{Error, Result, RoutingAgent, RoutingSink, StaticRoutingAgent};

pub(crate) use self::rib::RibBuilder;
pub(crate) use self::rib::{Decision, DispatchAction, Rib, RibSnapshot, RoutingKey};

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
