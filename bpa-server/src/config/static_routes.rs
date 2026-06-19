use std::{io::ErrorKind, path::PathBuf, sync::Arc};

use anyhow::Context;
use hardy_bpa::routes::RoutingAgent;
use serde::{Deserialize, Serialize};

use super::{WatchConfig, default_config_dir};
use crate::static_routes::StaticRoutesAgent;

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// Path to the routes file (default: `/etc/hardy/static_routes`).
    pub routes_file: PathBuf,
    /// Default route priority when not specified per-route (default: `100`).
    pub priority: u32,
    /// Watch the routes file for changes and reload automatically.
    /// Values: "native" (default), "poll" (works in Docker), "none" to disable.
    pub watch: WatchConfig,
    /// Protocol identifier used when registering with the BPA (default: `"static_routes"`).
    pub protocol_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            routes_file: default_config_dir().join("static_routes"),
            priority: 100,
            watch: WatchConfig::default(),
            protocol_id: "static_routes".to_string(),
        }
    }
}

impl Config {
    /// Resolves the routes file against the current directory and builds the
    /// static routing agent.
    pub fn build(&self) -> anyhow::Result<Arc<dyn RoutingAgent>> {
        let routes_file = std::env::current_dir()
            .context("Failed to get current directory")?
            .join(&self.routes_file);

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

        Ok(Arc::new(StaticRoutesAgent::new(
            routes_file,
            self.priority,
            self.watch.into(),
        )))
    }
}
