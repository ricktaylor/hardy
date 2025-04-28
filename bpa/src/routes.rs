use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, std::hash::Hash)]
pub enum Action {
    Drop(Option<bpv7::StatusReportReasonCode>), // Drop the bundle
    Forward,                                    // Forward to CLA
    Via(bpv7::Eid),                             // Recursive lookup
    Store(time::OffsetDateTime),                // Wait for later availability
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Drop(Some(reason)) => write!(f, "reject {:?}", reason),
            Self::Drop(None) => write!(f, "drop"),
            Self::Forward => write!(f, "forward"),
            Self::Via(eid) => write!(f, "via {eid}"),
            Self::Store(until) => write!(f, "store until {until}"),
        }
    }
}
