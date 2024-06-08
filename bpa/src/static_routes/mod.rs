use super::*;
use std::collections::HashMap;
use std::path::PathBuf;
use utils::settings;

const ID: &str = "static";

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
    routes: HashMap<bundle::EidPattern, StaticRoute>,
}

impl StaticRoutes {
    async fn init(
        mut self,
        _task_set: &mut tokio::task::JoinSet<()>,
        _cancel_token: tokio_util::sync::CancellationToken,
    ) {
        info!(
            "Loading static routes from '{}'",
            self.config.route_file.to_string_lossy()
        );

        self.refresh_routes().await.trace_expect(&format!(
            "Failed to read static_routes file '{}'",
            self.config.route_file.to_string_lossy()
        ));

        // Set up file watcher
        //let self_cloned = self.clone();
        //task_set.spawn_blocking(move || self_cloned.watch());
    }

    async fn refresh_routes(&mut self) -> Result<(), Error> {
        // Reload the routes
        let mut drop_routes = Vec::new();
        let mut add_routes = Vec::new();
        for r in parse::load_routes(&self.config.route_file).await? {
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
            self.fib.remove(ID, &k).await;
        }

        // Add routes
        for (k, v) in add_routes {
            if let Err(e) = self
                .fib
                .add(
                    ID.to_string(),
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

    /*fn watch(mut self) {
        todo!()
    }*/
}

#[instrument(skip_all)]
pub async fn init(
    config: &::config::Config,
    fib: fib::Fib,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    if let Some(config) =
        settings::get_with_default::<Option<config::Config>, _>(config, "static_routes", None)
            .trace_expect("Invalid 'static_routes' section in configuration")
    {
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
