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

// Default route priority when not specified per-route.
const DEFAULT_PRIORITY: u32 = 100;

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
    /// The protocol identifier the agent registers under when none is
    /// configured.
    pub const DEFAULT_PROTOCOL_ID: &str = "static_routes";

    /// Creates the agent. `None` applies the agent's own defaults: the
    /// `static_routes` file in the platform config directory, and priority
    /// `100`. `watch` is `None` to disable watching the routes file.
    pub fn new(
        routes_file: Option<PathBuf>,
        priority: Option<u32>,
        watch: Option<WatchMode>,
    ) -> Self {
        Self {
            routes_file: routes_file
                .unwrap_or_else(|| crate::config::default_config_dir().join("static_routes")),
            priority: priority.unwrap_or(DEFAULT_PRIORITY),
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
                    reload_routes(&routes_file, priority, &*sink, &routes).await;
                }
            })
            .await;
        });
    }
}

// Brings the installed routes in line with the file. A file that cannot be
// read or parsed leaves the installed routes as they are.
async fn reload_routes(
    routes_file: &Path,
    priority: u32,
    sink: &dyn RoutingSink,
    routes: &Mutex<Vec<StaticRoute>>,
) {
    let new_routes = match loader::load_routes(routes_file).await {
        Ok(routes) => routes,
        Err(e) => {
            error!("Failed to load static routes, keeping the installed routes: {e}");
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

#[cfg(test)]
mod tests {
    use std::fs;

    use hardy_bpa::routing::Result;
    use tempfile::TempDir;

    use super::*;

    const PRIORITY: u32 = 100;
    const ROUTES: &str = "ipn:2.*.* via ipn:2.1.0\n";

    #[derive(Debug, PartialEq)]
    enum Call {
        Add(EidPattern, RouteAction, u32),
        Remove(EidPattern, RouteAction, u32),
    }

    // Records every route change the agent makes, in order.
    #[derive(Default)]
    struct RecordingSink {
        calls: Mutex<Vec<Call>>,
    }

    #[hardy_async::async_trait]
    impl RoutingSink for RecordingSink {
        async fn unregister(&self) {}

        async fn add_route(
            &self,
            pattern: EidPattern,
            action: RouteAction,
            priority: u32,
        ) -> Result<bool> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Add(pattern, action, priority));
            Ok(true)
        }

        async fn remove_route(
            &self,
            pattern: &EidPattern,
            action: &RouteAction,
            priority: u32,
        ) -> Result<bool> {
            self.calls.lock().unwrap().push(Call::Remove(
                pattern.clone(),
                action.clone(),
                priority,
            ));
            Ok(true)
        }
    }

    fn add() -> Call {
        Call::Add(
            "ipn:2.*.*".parse().unwrap(),
            RouteAction::Via("ipn:2.1.0".parse().unwrap()),
            PRIORITY,
        )
    }

    fn remove() -> Call {
        Call::Remove(
            "ipn:2.*.*".parse().unwrap(),
            RouteAction::Via("ipn:2.1.0".parse().unwrap()),
            PRIORITY,
        )
    }

    // A routes file whose route is already installed through a recording sink.
    struct Fixture {
        _dir: TempDir,
        path: PathBuf,
        sink: RecordingSink,
        routes: Mutex<Vec<StaticRoute>>,
    }

    impl Fixture {
        async fn installed() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("static_routes");
            fs::write(&path, ROUTES).unwrap();
            let fixture = Self {
                _dir: dir,
                path,
                sink: RecordingSink::default(),
                routes: Mutex::new(Vec::new()),
            };
            fixture.reload().await;
            fixture
        }

        async fn reload(&self) {
            reload_routes(&self.path, PRIORITY, &self.sink, &self.routes).await;
        }

        // Restores the original file and reloads it: a route still tracked as
        // installed is not added again.
        async fn restore(self) -> Vec<Call> {
            fs::write(&self.path, ROUTES).unwrap();
            self.reload().await;
            self.sink.calls.into_inner().unwrap()
        }
    }

    #[tokio::test]
    async fn a_reload_that_fails_to_parse_keeps_the_routes() {
        let fixture = Fixture::installed().await;

        fs::write(&fixture.path, "ipn:2.*.* via Broken\n").unwrap();
        fixture.reload().await;

        assert_eq!(fixture.restore().await, vec![add()]);
    }

    #[tokio::test]
    async fn a_reload_that_fails_to_read_keeps_the_routes() {
        let fixture = Fixture::installed().await;

        // Not UTF-8, so the read itself fails.
        fs::write(&fixture.path, [0xff, 0xfe]).unwrap();
        fixture.reload().await;

        assert_eq!(fixture.restore().await, vec![add()]);
    }

    #[tokio::test]
    async fn a_deleted_routes_file_withdraws_its_routes() {
        let fixture = Fixture::installed().await;

        fs::remove_file(&fixture.path).unwrap();
        fixture.reload().await;

        assert_eq!(fixture.restore().await, vec![add(), remove(), add()]);
    }
}
