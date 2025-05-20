use super::*;

// DO NOT REORDER!!
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    Via(bpv7::Eid),                             // Recursive lookup
    Store(time::OffsetDateTime),                // Wait for later availability
    Drop(Option<bpv7::StatusReportReasonCode>), // Drop the bundle
}
