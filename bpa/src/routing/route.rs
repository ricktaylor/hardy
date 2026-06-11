use hardy_bpv7::eid::Eid;
use hardy_eid_patterns::EidPattern;

use super::table::RouteAction;

/// A routing rule as seen by agents: pattern + action + priority.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Route {
    pub pattern: EidPattern,
    pub action: RouteAction,
    pub priority: u32,
}

impl Route {
    pub fn via(pattern: EidPattern, next_hop: Eid, priority: u32) -> Self {
        Self {
            pattern,
            action: RouteAction::Via(next_hop),
            priority,
        }
    }

    pub fn drop(
        pattern: EidPattern,
        reason: Option<hardy_bpv7::status_report::ReasonCode>,
        priority: u32,
    ) -> Self {
        Self {
            pattern,
            action: RouteAction::Drop(reason),
            priority,
        }
    }

    pub fn reflect(pattern: EidPattern, priority: u32) -> Self {
        Self {
            pattern,
            action: RouteAction::Reflect,
            priority,
        }
    }
}

impl core::fmt::Display for Route {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} {} priority {}",
            self.pattern, self.action, self.priority
        )
    }
}
