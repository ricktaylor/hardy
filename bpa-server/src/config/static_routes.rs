use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::default_config_dir;
use crate::watcher::WatchMode;

fn default_watch() -> Option<WatchMode> {
    Some(WatchMode::Native)
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// Path to the routes file (default: `/etc/hardy/static_routes`).
    pub routes_file: PathBuf,
    /// Default route priority when not specified per-route (default: `100`).
    pub priority: u32,
    /// Watch the routes file for changes and reload automatically.
    /// Values: "native" (inotify/kqueue), "poll" (works in Docker). Default: "native".
    #[serde(default = "default_watch")]
    pub watch: Option<WatchMode>,
    /// Protocol identifier used when registering with the BPA (default: `"static_routes"`).
    pub protocol_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            routes_file: default_config_dir().join("static_routes"),
            priority: 100,
            watch: default_watch(),
            protocol_id: "static_routes".to_string(),
        }
    }
}
