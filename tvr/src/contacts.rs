use crate::scheduler::SchedulerHandle;
use hardy_bpa::routes::{Action, RoutingAgent, RoutingSink};
use hardy_bpv7::eid::NodeId;
use std::sync::Arc;
use tracing::{debug, info};

// A scheduled contact — the canonical internal representation used by
// both the file parser and gRPC session service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    // EID pattern that this contact matches against (e.g. `ipn:2.*.*`).
    pub pattern: hardy_eid_patterns::EidPattern,
    // Routing action to take for matching bundles (`via` or `drop`).
    pub action: Action,
    // Optional priority override; if `None`, the agent's default priority is used.
    pub priority: Option<u32>,
    // When this contact is active.
    pub schedule: Schedule,
    // Optional link bandwidth in bits per second.
    pub bandwidth_bps: Option<u64>,
    // Optional one-way link delay in microseconds.
    pub delay_us: Option<u32>,
}

// Time window for a contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    // Always active (like a static route).
    Permanent,

    // Active during a single time window.
    OneShot {
        start: Option<time::OffsetDateTime>,
        end: Option<time::OffsetDateTime>,
    },

    // Recurring via cron expression.
    Recurring {
        cron: crate::cron::CronExpr,
        duration: std::time::Duration,
        until: Option<time::OffsetDateTime>,
    },
}

// The TVR routing agent. Manages the RIB and projects active contacts
// into the BPA's FIB via the RoutingSink.
pub struct TvrAgent {
    default_priority: u32,
    scheduler: SchedulerHandle,
    sink: hardy_async::sync::spin::Once<Arc<dyn RoutingSink>>,
}

impl TvrAgent {
    // Create a new TVR agent with the given default contact priority
    // and a handle to the scheduler.
    pub fn new(default_priority: u32, scheduler: SchedulerHandle) -> Self {
        Self {
            default_priority,
            scheduler,
            sink: hardy_async::sync::spin::Once::new(),
        }
    }

    // Returns the default priority used for contacts without an explicit priority.
    pub fn default_priority(&self) -> u32 {
        self.default_priority
    }

    // Returns a reference to the scheduler handle for submitting contact operations.
    pub fn scheduler(&self) -> &SchedulerHandle {
        &self.scheduler
    }

    // Get the stored sink (available after registration).
    pub fn sink(&self) -> Option<Arc<dyn RoutingSink>> {
        self.sink.get().cloned()
    }

    // Explicitly unregister from the BPA.
    pub async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }
}

#[hardy_bpa::async_trait]
impl RoutingAgent for TvrAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, node_ids: &[NodeId]) {
        info!(
            "TVR agent registered, node IDs: {:?}",
            node_ids.iter().map(|n| n.to_string()).collect::<Vec<_>>()
        );
        let sink: Arc<dyn RoutingSink> = sink.into();
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        debug!("TVR agent unregistered");
    }
}
