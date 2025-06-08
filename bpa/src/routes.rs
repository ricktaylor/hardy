// DO NOT REORDER!!
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    Via(hardy_bpv7::eid::Eid),                           // Recursive lookup
    Store(time::OffsetDateTime),                         // Wait for later availability
    Drop(Option<hardy_bpv7::status_report::ReasonCode>), // Drop the bundle
}
