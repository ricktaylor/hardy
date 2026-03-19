use core::fmt::{Display, Formatter, Result};
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Drop(Option<ReasonCode>), // Drop the bundle
    Reflect,                  // Return to last hop
    Via(Eid),                 // Recursive lookup
}

impl Display for Action {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Action::Drop(reason_code) => {
                if let Some(reason) = reason_code {
                    write!(f, "Drop({reason:?})")
                } else {
                    write!(f, "Drop")
                }
            }
            Action::Reflect => write!(f, "Reflect"),
            Action::Via(eid) => write!(f, "Via {eid}"),
        }
    }
}
