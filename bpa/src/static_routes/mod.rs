use super::*;
use notify_debouncer_full::{
    new_debouncer,
    notify::{
        event::{CreateKind, RemoveKind},
        EventKind, RecursiveMode,
    },
    DebouncedEvent,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::*;

mod config;
mod parse;

#[derive(Debug, Clone, Eq, PartialEq)]
struct StaticRoute {
    priority: Option<u32>,
    action: fib::Action,
}

#[derive(Clone)]
pub struct StaticRoutes {
    config: config::Config,
    fib: fib::Fib,
    routes: HashMap<bpv7::EidPattern, StaticRoute>,
}

impl StaticRoutes {
    async fn init(
        mut self,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        info!(
            "Loading static routes from '{}'",
            self.config.routes_file.to_string_lossy()
        );

        self.refresh_routes(false)
            .await
            .trace_expect("Failed to process static routes file");

        if self.config.watch {
            info!("Monitoring static routes file for changes");

            // Set up file watcher
            self.watch(task_set, cancel_token);
        }
    }

    async fn refresh_routes(&mut self, ignore_errors: bool) -> Result<(), Error> {
        // Reload the routes
        let mut drop_routes = Vec::new();
        let mut add_routes = Vec::new();
        for r in
            parse::load_routes(&self.config.routes_file, ignore_errors, self.config.watch).await?
        {
            if let Some(v2) = self.routes.get(&r.0) {
                if &r.1 != v2 {
                    drop_routes.push(r.0.clone());
                    add_routes.push(r);
                }
            } else {
                add_routes.push(r);
            }
        }

        // Drop routes
        for k in drop_routes {
            self.routes.remove(&k);
            self.fib.remove(&self.config.protocol_id, &k).await;
        }

        // Add routes
        for (k, v) in add_routes {
            if let Err(e) = self
                .fib
                .add(
                    self.config.protocol_id.clone(),
                    &k,
                    v.priority.unwrap_or(self.config.priority),
                    v.action.clone(),
                )
                .await
            {
                error!("Failed to insert static route: {k:?}: {}", e.to_string());
            } else {
                self.routes.insert(k, v);
            }
        }
        Ok(())
    }

    fn watch(
        &self,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let routes_dir = self
            .config
            .routes_file
            .parent()
            .expect("Failed to get 'routes_file' parent directory!")
            .to_path_buf();
        let routes_file = self.config.routes_file.clone();

        let mut self_cloned = self.clone();
        task_set.spawn(async move {
            let (tx, mut rx) = channel(1);

            let mut debouncer = new_debouncer(Duration::from_secs(1), None, move |res| {
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
                                    info!("Reloading static routes from '{}'",routes_file.to_string_lossy());
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
                    _ = cancel_token.cancelled() => break
                }
            }
        });
    }
}

#[instrument(skip_all)]
pub async fn init(
    config: &::config::Config,
    fib: fib::Fib,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    if let Some(config) = config::Config::new(config) {
        StaticRoutes {
            config,
            fib,
            routes: HashMap::new(),
        }
        .init(task_set, cancel_token)
        .await;
    } else {
        info!("No static routes configured");
    }
}
