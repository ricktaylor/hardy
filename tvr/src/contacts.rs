use crate::scheduler::SchedulerHandle;
use hardy_bpa::routing::{Agent, Context, Factory, RouteAction, Sink, factory_fn};
use std::sync::Arc;
use tracing::info;

// A scheduled contact, in the representation shared by the contact plan file
// parser and the gRPC session service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    // Matched against the bundle destination, for example `ipn:2.*.*`.
    pub pattern: hardy_eid_patterns::EidPattern,
    // Applied to matching bundles: `via` or `drop`.
    pub action: RouteAction,
    // When `None`, the agent's default priority applies.
    pub priority: Option<u32>,
    pub schedule: Schedule,
    pub bandwidth_bps: Option<u64>,
    // One-way link delay.
    pub delay_us: Option<u32>,
}

// The period during which a contact is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    // Always active (like a static route).
    Permanent,

    // Active during a single time window.
    OneShot {
        start: Option<time::OffsetDateTime>,
        end: Option<time::OffsetDateTime>,
    },

    Recurring {
        cron: crate::cron::CronExpr,
        duration: std::time::Duration,
        until: Option<time::OffsetDateTime>,
    },
}

// The registration state of the TVR agent. The scheduler adds and removes the
// routes of each contact in the BPA's RIB through the sink, as the contact's
// window opens and closes.
pub struct TvrAgent {
    default_priority: u32,
    scheduler: SchedulerHandle,
    sink: Arc<dyn Sink>,
}

impl TvrAgent {
    // Creates a factory for the agent, bound to a default contact priority and
    // a scheduler handle.
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

    pub fn default_priority(&self) -> u32 {
        self.default_priority
    }

    pub fn scheduler(&self) -> &SchedulerHandle {
        &self.scheduler
    }

    pub fn sink(&self) -> &Arc<dyn Sink> {
        &self.sink
    }

    pub async fn unregister(&self) {
        self.sink.unregister().await;
    }
}

impl Agent for TvrAgent {}
