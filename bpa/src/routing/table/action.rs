use std::sync::Arc;

use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;

use crate::services;

/// What routing agents configure. Validated on insert into VirtualRouteTable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RouteAction {
    Via(Eid),
    Reflect,
    Drop(Option<ReasonCode>),
}

impl core::fmt::Display for RouteAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RouteAction::Drop(Some(reason)) => write!(f, "Drop({reason:?})"),
            RouteAction::Drop(None) => write!(f, "Drop"),
            RouteAction::Reflect => write!(f, "Reflect"),
            RouteAction::Via(eid) => write!(f, "Via {eid}"),
        }
    }
}

impl PartialOrd for RouteAction {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RouteAction {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let rank = |a: &RouteAction| -> u8 {
            match a {
                RouteAction::Drop(_) => 0,
                RouteAction::Reflect => 1,
                RouteAction::Via(_) => 2,
            }
        };
        match rank(self).cmp(&rank(other)) {
            core::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        match (self, other) {
            (RouteAction::Drop(a), RouteAction::Drop(b)) => a.cmp(b),
            (RouteAction::Via(a), RouteAction::Via(b)) => a.cmp(b),
            _ => core::cmp::Ordering::Equal,
        }
    }
}

/// BPA-internal actions. Always correct by construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum InternalAction {
    Forward(u32),
    Local(Arc<services::registry::Service>),
    AdminEndpoint,
}

impl core::fmt::Display for InternalAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InternalAction::Forward(peer) => write!(f, "CLA peer {peer}"),
            InternalAction::Local(service) => write!(f, "local service {}", service.service_id),
            InternalAction::AdminEndpoint => write!(f, "administrative endpoint"),
        }
    }
}

impl PartialOrd for InternalAction {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for InternalAction {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let rank = |a: &InternalAction| -> u8 {
            match a {
                InternalAction::AdminEndpoint => 0,
                InternalAction::Local(_) => 1,
                InternalAction::Forward(_) => 2,
            }
        };
        match rank(self).cmp(&rank(other)) {
            core::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        match (self, other) {
            (InternalAction::Local(a), InternalAction::Local(b)) => a.cmp(b),
            (InternalAction::Forward(a), InternalAction::Forward(b)) => a.cmp(b),
            _ => core::cmp::Ordering::Equal,
        }
    }
}

/// Combined action stored in the route table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Action {
    Route(RouteAction),
    Internal(InternalAction),
}

impl core::fmt::Display for Action {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Action::Route(a) => a.fmt(f),
            Action::Internal(a) => a.fmt(f),
        }
    }
}

impl PartialOrd for Action {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Action {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Drop < AdminEndpoint < Local < Forward < Reflect < Via
        let rank = |a: &Action| -> u8 {
            match a {
                Action::Route(RouteAction::Drop(_)) => 0,
                Action::Internal(InternalAction::AdminEndpoint) => 1,
                Action::Internal(InternalAction::Local(_)) => 2,
                Action::Internal(InternalAction::Forward(_)) => 3,
                Action::Route(RouteAction::Reflect) => 4,
                Action::Route(RouteAction::Via(_)) => 5,
            }
        };
        match rank(self).cmp(&rank(other)) {
            core::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        match (self, other) {
            (Action::Route(a), Action::Route(b)) => a.cmp(b),
            (Action::Internal(a), Action::Internal(b)) => a.cmp(b),
            _ => core::cmp::Ordering::Equal,
        }
    }
}
