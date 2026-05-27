use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hardy_async::TaskPool;
use hardy_async::watcher::{self, WatchMode};
use hardy_bpa::routes::{Action, RoutingAgent, RoutingContext};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use tracing::{error, info};

use super::loader;

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct StaticRoute {
    pub(super) pattern: EidPattern,
    pub(super) priority: Option<u32>,
    pub(super) action: Action,
}

pub struct StaticRoutesAgent {
    routes_file: PathBuf,
    priority: u32,
    watch: Option<WatchMode>,
    ctx: hardy_async::sync::spin::Once<RoutingContext>,
    routes: Mutex<Vec<StaticRoute>>,
    tasks: TaskPool,
}

impl StaticRoutesAgent {
    pub fn new(routes_file: PathBuf, priority: u32, watch: Option<WatchMode>) -> Self {
        Self {
            routes_file,
            priority,
            watch,
            ctx: hardy_async::sync::spin::Once::new(),
            routes: Mutex::new(Vec::new()),
            tasks: TaskPool::new(),
        }
    }

    fn start_watcher(&self, mode: WatchMode) {
        let watch_path = self.routes_file.clone();
        let routes_file = self.routes_file.clone();
        let priority = self.priority;
        let ctx = self.ctx.get().unwrap().clone();
        let routes = self.routes.lock().unwrap();
        let routes_vec = routes.clone();
        drop(routes);
        let routes_mutex = std::sync::Arc::new(Mutex::new(routes_vec));
        let cancel = self.tasks.cancel_token().clone();

        // Re-share the routes mutex for the watcher
        let routes_ref = routes_mutex.clone();

        hardy_async::spawn!(self.tasks, "static_routes_watcher", async move {
            watcher::watch(&watch_path, mode, cancel, move || {
                let routes_file = routes_file.clone();
                let ctx = ctx.clone();
                let routes = routes_ref.clone();
                async move {
                    info!("Reloading static routes from '{}'", routes_file.display());
                    reload_routes(&routes_file, priority, &ctx, &routes, true).await;
                }
            })
            .await;
        });
    }
}

async fn reload_routes(
    routes_file: &Path,
    priority: u32,
    ctx: &RoutingContext,
    routes: &Mutex<Vec<StaticRoute>>,
    ignore_errors: bool,
) {
    let new_routes = match loader::load_routes(routes_file, ignore_errors, true).await {
        Ok(routes) => routes,
        Err(e) => {
            error!("Failed to load static routes: {e}");
            return;
        }
    };

    let (to_remove, to_add) = {
        let routes = routes.lock().unwrap();
        let to_remove: Vec<_> = routes
            .iter()
            .filter(|r| !new_routes.iter().any(|r2| *r == r2))
            .cloned()
            .collect();
        let to_add: Vec<_> = new_routes
            .into_iter()
            .filter(|r| !routes.iter().any(|r2| r == r2))
            .collect();
        (to_remove, to_add)
    };

    for r in &to_remove {
        ctx.remove_route(&r.pattern, &r.action, r.priority.unwrap_or(priority));
    }

    for r in &to_add {
        ctx.add_route(
            r.pattern.clone(),
            r.action.clone(),
            r.priority.unwrap_or(priority),
        );
    }

    {
        let mut routes = routes.lock().unwrap();
        routes.retain(|r| !to_remove.contains(r));
        for r in to_add {
            routes.push(r);
        }
    }
}

#[hardy_async::async_trait]
impl RoutingAgent for StaticRoutesAgent {
    async fn on_register(&self, ctx: RoutingContext, _node_ids: &[NodeId]) {
        self.ctx.call_once(|| ctx);

        info!(
            "Loading static routes from '{}'",
            self.routes_file.display()
        );

        reload_routes(
            &self.routes_file,
            self.priority,
            self.ctx.get().unwrap(),
            &self.routes,
            false,
        )
        .await;

        if let Some(mode) = self.watch {
            self.start_watcher(mode);
        }
    }

    async fn on_unregister(&self) {
        self.tasks.shutdown().await;
    }
}
