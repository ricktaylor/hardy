pub(crate) mod action;
pub mod agent;
pub(crate) mod rib;
pub(crate) mod table;

pub use self::action::RouteAction;
pub use self::agent::{Error, Result, RoutingAgent, RoutingSink, StaticRoutingAgent};
pub(crate) use self::rib::RibBuilder;
pub use self::rib::{DispatchAction, Rib};
