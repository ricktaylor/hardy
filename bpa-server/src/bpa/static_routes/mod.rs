use anyhow::Context;
use hardy_async::TaskPool;
use hardy_async::sync::spin::Once;
use hardy_bpa::routes::{Action, RoutingAgent, RoutingSink};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use notify_debouncer_full::notify::event::{CreateKind, RemoveKind};
use notify_debouncer_full::notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebouncedEvent, new_debouncer};
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use trace_err::*;
use tracing::{error, info};

use crate::config::default_config_dir;

mod parse;

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
struct StaticRoute {
    pattern: EidPattern,
    priority: Option<u32>,
    action: Action,
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
    fn new(routes_file: PathBuf, priority: u32, watch: bool) -> Self {
        Self {
            routes_file,
            priority,
            watch,
            sink: Once::new(),
            routes: Arc::new(Mutex::new(Vec::new())),
            tasks: TaskPool::new(),
        }
    }

    async fn refresh_routes(&self, ignore_errors: bool) {
        let sink = match self.sink.get() {
            Some(sink) => sink,
            None => return,
        };

        let new_routes =
            match parse::load_routes(&self.routes_file, ignore_errors, self.watch).await {
                Ok(routes) => routes,
                Err(e) => {
                    error!("Failed to load static routes: {e}");
                    return;
                }
            };

        // Compute diff under lock (no awaits while holding lock)
        let (to_remove, to_add) = {
            let routes = self.routes.lock().unwrap();
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

        // Apply removals (no lock held)
        for r in &to_remove {
            sink.remove_route(&r.pattern, &r.action, r.priority.unwrap_or(self.priority))
                .await
                .ok();
        }

        // Apply additions (no lock held)
        for r in &to_add {
            sink.add_route(
                r.pattern.clone(),
                r.action.clone(),
                r.priority.unwrap_or(self.priority),
            )
            .await
            .ok();
        }

        // Update internal state
        {
            let mut routes = self.routes.lock().unwrap();
            routes.retain(|r| !to_remove.contains(r));
            for r in to_add {
                routes.push(r);
            }
        }
    }

    fn start_watcher(&self) {
        let routes_dir = self
            .routes_file
            .parent()
            .trace_expect("Failed to get 'routes_file' parent directory!")
            .to_path_buf();
        let routes_file = self.routes_file.clone();
        let priority = self.priority;
        let sink = self.sink.get().unwrap().clone();
        let routes = self.routes.clone();
        let watch = self.watch;
        let cancel_token = self.tasks.cancel_token().clone();

        hardy_async::spawn!(self.tasks, "static_routes_watcher", async move {
            let (tx, rx) = flume::unbounded();
            let mut debouncer = new_debouncer(Duration::from_secs(1), None, move |res| match res {
                Ok(events) => {
                    for e in events {
                        if tx.send(e).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    for e in e {
                        error!("Watch error: {e}")
                    }
                }
            })
            .trace_expect("Failed to create directory watcher");

            debouncer
                .watch(&routes_dir, RecursiveMode::NonRecursive)
                .trace_expect("Failed to watch file");

            loop {
                tokio::select! {
                    res = rx.recv_async() => match res {
                        Err(_) => break,
                        Ok(DebouncedEvent{ event, .. }) => {
                            if match event.kind {
                                EventKind::Create(CreateKind::File)|
                                EventKind::Modify(_)|
                                EventKind::Remove(RemoveKind::File) => {
                                    event.paths.iter().any(|p| p == &routes_file)
                                }
                                _ => false
                            } {
                                info!("Reloading static routes from '{}' (event: {:?})", routes_file.display(), event.kind);
                                refresh_routes_inner(&routes_file, priority, &*sink, &routes, watch).await;
                            }
                        },
                    },
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        });
    }
}

// Standalone refresh function for use in the watcher task (avoids needing &self).
async fn refresh_routes_inner(
    routes_file: &Path,
    priority: u32,
    sink: &dyn RoutingSink,
    routes: &Mutex<Vec<StaticRoute>>,
    watch: bool,
) {
    let new_routes = match parse::load_routes(routes_file, true, watch).await {
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

        self.refresh_routes(false).await;

        if self.watch {
            info!("Monitoring static routes file for changes");
            self.start_watcher();
        }
    }

    async fn on_unregister(&self) {
        self.tasks.shutdown().await;
    }
}

pub fn new(
    routes_file: PathBuf,
    priority: u32,
    watch: bool,
) -> anyhow::Result<Arc<dyn RoutingAgent>> {
    let routes_file = std::env::current_dir()
        .context("Failed to get current directory")?
        .join(routes_file);

    let routes_file = match routes_file.canonicalize() {
        Ok(path) => path,
        Err(e) => {
            if e.kind() != ErrorKind::NotFound {
                return Err(anyhow::anyhow!(
                    "Failed to canonicalise routes_file '{}': {e}'",
                    routes_file.display()
                ));
            }
            routes_file
        }
    };

    let agent: Arc<dyn RoutingAgent> =
        Arc::new(StaticRoutesAgent::new(routes_file, priority, watch));

    Ok(agent)
}
