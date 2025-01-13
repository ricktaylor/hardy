use super::*;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Drop(Option<bpv7::StatusReportReasonCode>), // Drop the bundle
    Via(bpv7::Eid),                             // Recursive lookup
    Store(time::OffsetDateTime),                // Wait for later availability
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Drop(reason) => {
                if let Some(reason) = reason {
                    write!(f, "drop({:?})", reason)
                } else {
                    write!(f, "drop")
                }
            }
            Self::Via(eid) => write!(f, "via {eid}"),
            Self::Store(until) => write!(f, "store until {until}"),
        }
    }
}
