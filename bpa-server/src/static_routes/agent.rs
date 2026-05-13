use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hardy_async::TaskPool;
use hardy_async::sync::spin::Once;
use hardy_bpa::routes::{Action, RoutingAgent, RoutingSink};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::config::default_config_dir;
use crate::watcher;

use super::loader;

// Configuration for the static routes routing agent.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    // Path to the routes file (default: `/etc/hardy/static_routes`).
    pub routes_file: PathBuf,
    // Default route priority when not specified per-route (default: `100`).
    pub priority: u32,
    // Watch the routes file for changes and reload automatically (default: `true`).
    pub watch: bool,
    // Protocol identifier used when registering with the BPA (default: `"static_routes"`).
    pub protocol_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            routes_file: default_config_dir().join("static_routes"),
            priority: 100,
            watch: true,
            protocol_id: "static_routes".to_string(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct StaticRoute {
    pub(super) pattern: EidPattern,
    pub(super) priority: Option<u32>,
    pub(super) action: Action,
}

pub struct StaticRoutesAgent {
    routes_file: PathBuf,
    priority: u32,
    watch: bool,
    sink: Once<Arc<dyn RoutingSink>>,
    routes: Arc<Mutex<Vec<StaticRoute>>>,
    tasks: TaskPool,
}

impl StaticRoutesAgent {
    pub fn new(routes_file: PathBuf, priority: u32, watch: bool) -> Self {
        Self {
            routes_file,
            priority,
            watch,
            sink: Once::new(),
            routes: Arc::new(Mutex::new(Vec::new())),
            tasks: TaskPool::new(),
        }
    }

    fn start_watcher(&self) {
        let watch_path = self.routes_file.clone();
        let routes_file = self.routes_file.clone();
        let priority = self.priority;
        let sink = self.sink.get().unwrap().clone();
        let routes = self.routes.clone();
        let cancel = self.tasks.cancel_token().clone();

        hardy_async::spawn!(self.tasks, "static_routes_watcher", async move {
            watcher::watch(&watch_path, cancel, move || {
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

/// Load routes from file, diff against current state, and apply changes via the sink.
///
/// Standalone function because the watcher closure cannot hold `&self`.
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

    // Compute diff under lock (no awaits while holding lock)
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

    for r in &to_add {
        sink.add_route(
            r.pattern.clone(),
            r.action.clone(),
            r.priority.unwrap_or(priority),
        )
        .await
        .ok();
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

        if self.watch {
            info!("Monitoring static routes file for changes");
            self.start_watcher();
        }
    }

    async fn on_unregister(&self) {
        self.tasks.shutdown().await;
    }
}
