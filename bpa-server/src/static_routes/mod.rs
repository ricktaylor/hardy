use super::*;
use hardy_bpa::routes::Action;
use hardy_eid_pattern as eid_pattern;
use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{
        EventKind, RecursiveMode,
        event::{CreateKind, RemoveKind},
    },
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

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
    priority: Option<u32>,
    action: Action,
}

#[derive(Clone)]
pub struct StaticRoutes {
    config: Config,
    bpa: Arc<hardy_bpa::bpa::Bpa>,
    routes: HashMap<eid_pattern::EidPattern, StaticRoute>,
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
        let mut drop_routes = Vec::new();
        let mut add_routes = Vec::new();
        for (pattern, r) in
            parse::load_routes(&self.config.routes_file, ignore_errors, self.config.watch).await?
        {
            if let Some(v2) = self.routes.get(&pattern) {
                if &r != v2 {
                    drop_routes.push((pattern.clone(), r.clone()));
                    add_routes.push((pattern, r));
                }
            } else {
                add_routes.push((pattern, r));
            }
        }

        // Drop routes
        for (k, v) in drop_routes {
            self.routes.remove(&k);
            self.bpa
                .remove_route(
                    &self.config.protocol_id,
                    &k,
                    &v.action,
                    v.priority.unwrap_or(self.config.priority),
                )
                .await;
        }

        // Add routes
        for (k, v) in add_routes {
            self.bpa
                .add_route(
                    self.config.protocol_id.clone(),
                    k.clone(),
                    v.action.clone(),
                    v.priority.unwrap_or(self.config.priority),
                )
                .await;
            self.routes.insert(k, v);
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
            .expect("Failed to get 'routes_file' parent directory!")
            .to_path_buf();
        let routes_file = self.config.routes_file.clone();

        let mut self_cloned = self.clone();
        let cancel_token = cancel_token.clone();
        task_tracker.spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);

            let mut debouncer = new_debouncer(std::time::Duration::from_secs(1), None, move |res| {
                tx.blocking_send(res)
                    .trace_expect("Failed to send notification")
            })
            .trace_expect("Failed to create file watcher");

            debouncer
                .watch(&routes_dir, RecursiveMode::NonRecursive)
                .trace_expect("Failed to watch file");

            loop {
                tokio::select! {
                    res = rx.recv() => match res {
                        None => break,
                        Some(Ok(events)) => {
                            for DebouncedEvent{ event, .. } in events {
                                if match event.kind {
                                    EventKind::Create(CreateKind::File)|
                                    EventKind::Modify(_)|
                                    EventKind::Remove(RemoveKind::File) => {
                                        info!("Detected change in static routes file: {:?}, looking for {:?}", event.paths, routes_file);
                                        event.paths.iter().any(|p| p == &routes_file)
                                    }
                                    _ => false
                                } {
                                    info!("Reloading static routes from '{}'",routes_file.display());
                                    self_cloned.refresh_routes(false).await.trace_expect("Failed to process static routes file");
                                }
                            }
                        },
                        Some(Err(errors)) => {
                            for err in errors {
                                error!("Watch error: {:?}", err)
                            }
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        rx.close();
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
    config.routes_file = 
        std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("Failed to get current directory: {e}"))?
        .join(&config.routes_file);

    // Try to create canonical file path
    config.routes_file = config.routes_file.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "Failed to canonicalise routes_file '{}': {e}'",
            config.routes_file.display()
        )
    })?;

    StaticRoutes {
        config,
        bpa: bpa.clone(),
        routes: HashMap::new(),
    }
    .init(cancel_token, task_tracker)
    .await
}
