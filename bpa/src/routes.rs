use super::*;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Action {
    Drop(Option<bpv7::StatusReportReasonCode>), // Drop the bundle
    Via(bpv7::Eid),                             // Recursive lookup
    Store(time::OffsetDateTime),                // Wait for later availability
}
