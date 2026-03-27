use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Bundle processing state machine.
///
/// # State Transitions
///
/// ```text
/// ┌─────────────────────────────────────────────────────────────────────────────┐
/// │                         BPv7 Bundle State Machine                          │
/// └─────────────────────────────────────────────────────────────────────────────┘
///
///  [Receive bundle]
///       │
///       ▼
///     New ──────────────────────────────────────────────────► [drop: filter rejected]
///       │ ingress filter passes; checkpoint before routing
///       ▼
///  Dispatching ─────────────────────────────────────────────► [drop: no route / TTL]
///       │
///       ├──► ForwardPending { peer, queue } ──► [sent to CLA; tombstone on ack]
///       │        (CLA unavailable → Waiting)
///       │
///       ├──► AduFragment { source, timestamp }
///       │        (all fragments arrived → reassembled → re-enter Dispatching)
///       │        (fragment missing → stay here until next fragment arrives)
///       │
///       ├──► Waiting
///       │        (no route yet; re-dispatched when RIB changes)
///       │
///       └──► WaitingForService { service }
///                (local service not registered yet; re-dispatched on registration)
/// ```
///
/// # Crash safety
///
/// The transition `New → Dispatching` is persisted **before** running the ingress
/// filter so that after a restart the bundle resumes from routing, not re-filtering.
/// All other transitions are similarly checkpointed before the action they guard.
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
