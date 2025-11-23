use super::*;
use hardy_bpa::routes::Action;
use hardy_eid_patterns as eid_patterns;
use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{
        EventKind, RecursiveMode,
        event::{CreateKind, RemoveKind},
    },
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod parse;

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    pub routes_file: PathBuf,
    pub priority: u32,
    pub watch: bool,
    pub protocol_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            routes_file: config::config_dir().join("static_routes"),
            priority: 100,
            watch: true,
            protocol_id: "static_routes".to_string(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct StaticRoute {
    pattern: eid_patterns::EidPattern,
    priority: Option<u32>,
    action: Action,
}

#[derive(Clone)]
pub struct StaticRoutes {
    config: Config,
    bpa: Arc<hardy_bpa::bpa::Bpa>,
    routes: Vec<StaticRoute>,
}

impl StaticRoutes {
    async fn init(
        mut self,
        cancel_token: &tokio_util::sync::CancellationToken,
        task_tracker: &tokio_util::task::TaskTracker,
    ) -> anyhow::Result<()> {
        info!(
            "Loading static routes from '{}'",
            self.config.routes_file.display()
        );

        self.refresh_routes(false).await?;

        if self.config.watch {
            info!("Monitoring static routes file for changes");

            // Set up file watcher
            self.watch(cancel_token, task_tracker);
        }

        Ok(())
    }

    async fn refresh_routes(&mut self, ignore_errors: bool) -> anyhow::Result<()> {
        // Reload the routes
        let mut new_routes =
            parse::load_routes(&self.config.routes_file, ignore_errors, self.config.watch).await?;

        // Calculate routes to drop (present in current but missing or changed in new) and drop routes
        for r in self
            .routes
            .extract_if(.., |r| !new_routes.iter().any(|r2| r == r2))
        {
            self.bpa
                .remove_route(
                    &self.config.protocol_id,
                    &r.pattern,
                    &r.action,
                    r.priority.unwrap_or(self.config.priority),
                )
                .await;
        }

        // Calculate routes to add (present in new but missing or changed in current)
        new_routes.retain(|r| self.routes.iter().any(|r2| r != r2));

        // Add routes
        for r in new_routes {
            self.bpa
                .add_route(
                    self.config.protocol_id.clone(),
                    r.pattern.clone(),
                    r.action.clone(),
                    r.priority.unwrap_or(self.config.priority),
                )
                .await;
            self.routes.push(r);
        }
        Ok(())
    }

    fn watch(
        &self,
        cancel_token: &tokio_util::sync::CancellationToken,
        task_tracker: &tokio_util::task::TaskTracker,
    ) {
        let routes_dir = self
            .config
            .routes_file
            .parent()
            .trace_expect("Failed to get 'routes_file' parent directory!")
            .to_path_buf();
        let routes_file = self.config.routes_file.clone();

        let mut self_cloned = self.clone();
        let cancel_token = cancel_token.clone();
        task_tracker.spawn(async move {
            let (tx, rx) = flume::unbounded();
            let mut debouncer =
                new_debouncer(
                    std::time::Duration::from_secs(1),
                    None,
                    move |res| match res {
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
                    },
                )
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
                                self_cloned.refresh_routes(false).await.trace_expect("Failed to process static routes file");
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

pub async fn init(
    mut config: Config,
    bpa: &Arc<hardy_bpa::bpa::Bpa>,
    cancel_token: &tokio_util::sync::CancellationToken,
    task_tracker: &tokio_util::task::TaskTracker,
) -> anyhow::Result<()> {
    // Ensure it's absolute
    config.routes_file = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to get current directory: {e}"))?
        .join(&config.routes_file);

    // Try to create canonical file path
    config.routes_file = match config.routes_file.canonicalize() {
        Ok(path) => path,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(anyhow::anyhow!(
                    "Failed to canonicalise routes_file '{}': {e}'",
                    config.routes_file.display()
                ));
            }
            config.routes_file
        }
    };

    StaticRoutes {
        config,
        bpa: bpa.clone(),
        routes: Vec::new(),
    }
    .init(cancel_token, task_tracker)
    .await
}
