use hardy_bpa::routes::{Action, RoutingAgent, RoutingSink};
use hardy_bpv7::eid::NodeId;
use tracing::{debug, info};

/// A scheduled contact — the canonical internal representation used by
/// both the file parser and gRPC session service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub pattern: hardy_eid_patterns::EidPattern,
    pub action: Action,
    pub priority: Option<u32>,
    pub schedule: Schedule,
    pub bandwidth_bps: Option<u64>,
    pub delay_us: Option<u32>,
}

/// Time window for a contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Always active (like a static route).
    Permanent,

    /// Active during a single time window.
    OneShot {
        start: Option<time::OffsetDateTime>,
        end: Option<time::OffsetDateTime>,
    },

    /// Recurring via cron expression.
    Recurring {
        cron: String,
        duration: std::time::Duration,
        until: Option<time::OffsetDateTime>,
    },
}

/// The TVR routing agent. Manages the RIB and projects active contacts
/// into the BPA's FIB via the RoutingSink.
pub struct TvrAgent {
    default_priority: u32,
    sink: hardy_async::sync::spin::Once<Box<dyn RoutingSink>>,
}

impl TvrAgent {
    pub fn new(default_priority: u32) -> Self {
        Self {
            default_priority,
            sink: hardy_async::sync::spin::Once::new(),
        }
    }

    pub fn default_priority(&self) -> u32 {
        self.default_priority
    }

    /// Explicitly unregister from the BPA.
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
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        debug!("TVR agent unregistered");
    }
}
