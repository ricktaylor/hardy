use crate::scheduler::SchedulerHandle;
use hardy_bpa::routing::{Agent, Context, Factory, RouteAction, Sink, factory_fn};
use std::sync::Arc;
use tracing::info;

// A scheduled contact — the canonical internal representation used by
// both the file parser and gRPC session service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    // EID pattern that this contact matches against (e.g. `ipn:2.*.*`).
    pub pattern: hardy_eid_patterns::EidPattern,
    // Routing action to take for matching bundles (`via` or `drop`).
    pub action: RouteAction,
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
// into the BPA's FIB via the routing sink.
pub struct TvrAgent {
    default_priority: u32,
    scheduler: SchedulerHandle,
    sink: Arc<dyn Sink>,
}

impl TvrAgent {
    // Builds the TVR agent with the given default contact priority and a
    // handle to the scheduler.
    pub fn factory(
        default_priority: u32,
        scheduler: SchedulerHandle,
    ) -> impl Factory<Component = Self> {
        factory_fn(move |cx: Context| {
            info!("TVR agent registered with a BPA running as {}", cx.node_ids);
            Ok(Arc::new(Self {
                default_priority,
                scheduler,
                sink: cx.sink,
            }))
        })
    }

    // Returns the default priority used for contacts without an explicit priority.
    pub fn default_priority(&self) -> u32 {
        self.default_priority
    }

    // Returns a reference to the scheduler handle for submitting contact operations.
    pub fn scheduler(&self) -> &SchedulerHandle {
        &self.scheduler
    }

    // The channel the agent projects its active contacts through.
    pub fn sink(&self) -> &Arc<dyn Sink> {
        &self.sink
    }

    // Explicitly unregister from the BPA.
    pub async fn unregister(&self) {
        self.sink.unregister().await;
    }
}

impl Agent for TvrAgent {}
