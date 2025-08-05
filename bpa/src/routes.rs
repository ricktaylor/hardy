#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Drop(Option<hardy_bpv7::status_report::ReasonCode>), // Drop the bundle
    Store(time::OffsetDateTime),                         // Wait for later availability
    Via(hardy_bpv7::eid::Eid),                           // Recursive lookup
}
