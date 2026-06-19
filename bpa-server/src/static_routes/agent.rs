use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use hardy_async::{
    TaskPool,
    sync::spin::Once,
    watcher::{self, WatchMode},
};
use hardy_bpa::routing::{RouteAction, RoutingAgent, RoutingSink};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use tracing::{error, info, warn};

use super::loader;

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct StaticRoute {
    pub(super) pattern: EidPattern,
    pub(super) priority: Option<u32>,
    pub(super) action: RouteAction,
}

pub struct StaticRoutesAgent {
    routes_file: PathBuf,
    priority: u32,
    watch: Option<WatchMode>,
    sink: Once<Arc<dyn RoutingSink>>,
    routes: Arc<Mutex<Vec<StaticRoute>>>,
    tasks: TaskPool,
}

impl StaticRoutesAgent {
    pub fn new(routes_file: PathBuf, priority: u32, watch: Option<WatchMode>) -> Self {
        Self {
            routes_file,
            priority,
            watch,
            sink: Once::new(),
            routes: Arc::new(Mutex::new(Vec::new())),
            tasks: TaskPool::new(),
        }
    }

    fn start_watcher(&self, mode: WatchMode) {
        let watch_path = self.routes_file.clone();
        let routes_file = self.routes_file.clone();
        let priority = self.priority;
        let sink = self.sink.get().unwrap().clone();
        let routes = self.routes.clone();
        let cancel = self.tasks.cancel_token().clone();

        hardy_async::spawn!(self.tasks, "static_routes_watcher", async move {
            watcher::watch(&watch_path, mode, cancel, move || {
                let routes_file = routes_file.clone();
                let sink = sink.clone();
                let routes = routes.clone();
                async move {
                    info!("Reloading static routes from '{}'", routes_file.display());
                    reload_routes(&routes_file, priority, &*sink, &routes, true).await;
                }
            })
            .await;
        });
    }
}

async fn reload_routes(
    routes_file: &Path,
    priority: u32,
    sink: &dyn RoutingSink,
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
        sink.remove_route(&r.pattern, &r.action, r.priority.unwrap_or(priority))
            .await
            .ok();
    }

    // Rejected routes are deliberately kept out of the tracked set so each
    // reload re-attempts and re-warns, instead of silently treating a route
    // the RIB refused as installed.
    let mut accepted = Vec::new();
    for r in to_add {
        match sink
            .add_route(
                r.pattern.clone(),
                r.action.clone(),
                r.priority.unwrap_or(priority),
            )
            .await
        {
            Ok(_) => accepted.push(r),
            Err(e) => warn!("Rejected static route {} => {}: {e}", r.pattern, r.action),
        }
    }

    {
        let mut routes = routes.lock().unwrap();
        routes.retain(|r| !to_remove.contains(r));
        routes.extend(accepted);
    }
}

#[hardy_async::async_trait]
impl RoutingAgent for StaticRoutesAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        let sink: Arc<dyn RoutingSink> = sink.into();
        self.sink.call_once(|| sink);

        info!(
            "Loading static routes from '{}'",
            self.routes_file.display()
        );

        reload_routes(
            &self.routes_file,
            self.priority,
            &**self.sink.get().unwrap(),
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
