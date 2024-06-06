use super::*;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub struct Config {
    #[serde(default = "Config::default_path")]
    pub route_file: PathBuf,

    #[serde(default = "Config::default_priority")]
    pub priority: u32,
}

impl Config {
    fn default_path() -> PathBuf {
        settings::config_dir().join("static_routes")
    }

    fn default_priority() -> u32 {
        100
    }
}
