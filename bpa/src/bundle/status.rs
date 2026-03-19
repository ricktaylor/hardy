use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum BundleStatus {
    #[default]
    New,
    Dispatching,
    ForwardPending {
        peer: u32,
        queue: Option<u32>,
    },
    AduFragment {
        source: Eid,
        timestamp: CreationTimestamp,
    },
    Waiting,
    WaitingForService {
        service: Eid,
    },
}
