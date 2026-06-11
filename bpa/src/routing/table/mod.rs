pub(crate) mod action;
mod atomic;
mod r#virtual;

use std::collections::BTreeMap;
use std::sync::Arc;

use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use hardy_eid_patterns::EidPattern;

use crate::services;

pub use action::RouteAction;
pub(crate) use action::{Action, InternalAction};

// priority -> [(pattern, action, source)]
pub(super) type Entries = BTreeMap<u32, Vec<(EidPattern, Action, String)>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Via route with null next-hop: {pattern}")]
    NullNextHop { pattern: EidPattern },
    #[error("Self-referential route: {pattern} via {next_hop}")]
    SelfReferential { pattern: EidPattern, next_hop: Eid },
    #[error("Route via own node: {pattern} via {next_hop}")]
    ViaOwnNode { pattern: EidPattern, next_hop: Eid },
    #[error("Reflect route matches own node: {pattern}")]
    ReflectMatchesSelf { pattern: EidPattern },
    #[error("Transitive loop: {}", chain.iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" -> "))]
    TransitiveLoop { chain: Vec<Eid> },
}

#[derive(Debug)]
pub(crate) enum FindResult<'a> {
    AdminEndpoint,
    Deliver(Arc<services::registry::Service>),
    Forward(Vec<(u32, &'a Eid)>),
    Drop(Option<ReasonCode>),
    Reflect,
}

pub(crate) use atomic::AtomicRouteTable;
pub(crate) use r#virtual::VirtualRouteTable;

/// Insert into a sorted vec, maintaining sort order by peer id. Skips duplicates.
pub(super) fn sorted_insert<'a>(peers: &mut Vec<(u32, &'a Eid)>, peer: u32, next_hop: &'a Eid) {
    if let Err(idx) = peers.binary_search_by_key(&peer, |(p, _)| *p) {
        peers.insert(idx, (peer, next_hop));
    }
}
