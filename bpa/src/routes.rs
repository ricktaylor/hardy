#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Drop(Option<hardy_bpv7::status_report::ReasonCode>), // Drop the bundle
    Store(time::OffsetDateTime),                         // Wait for later availability
    Via(hardy_bpv7::eid::Eid),                           // Recursive lookup
}

impl core::fmt::Display for Action {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Action::Drop(reason_code) => {
                if let Some(reason) = reason_code {
                    write!(f, "Drop({reason:?})")
                } else {
                    write!(f, "Drop")
                }
            }
            Action::Store(until) => write!(f, "wait until {until}"),
            Action::Via(eid) => write!(f, "via {eid}"),
        }
    }
}
