use flume::Sender;
use hardy_async::CancellationToken;
use hardy_eid_patterns::EidPattern;

use crate::routes::Action;

pub enum RouteOp {
    Add {
        pattern: EidPattern,
        action: Action,
        priority: u32,
    },
    Remove {
        pattern: EidPattern,
        action: Action,
        priority: u32,
    },
}

#[derive(Clone)]
pub struct RoutingContext {
    routes: Sender<RouteOp>,
    shutdown: CancellationToken,
}

impl RoutingContext {
    pub fn new(routes: Sender<RouteOp>, shutdown: CancellationToken) -> Self {
        Self { routes, shutdown }
    }

    pub fn add_route(&self, pattern: EidPattern, action: Action, priority: u32) {
        let _ = self.routes.send(RouteOp::Add {
            pattern,
            action,
            priority,
        });
    }

    pub fn remove_route(&self, pattern: &EidPattern, action: &Action, priority: u32) {
        let _ = self.routes.send(RouteOp::Remove {
            pattern: pattern.clone(),
            action: action.clone(),
            priority,
        });
    }

    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.shutdown
    }

    pub fn is_connected(&self) -> bool {
        !self.routes.is_disconnected()
    }
}
