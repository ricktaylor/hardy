use super::*;
use serde::Deserialize;
use utils::settings;

#[derive(Clone, Deserialize)]
pub struct Config {
    #[serde(default = "Config::default_path")]
    pub routes_file: PathBuf,

    #[serde(default = "Config::default_priority")]
    pub priority: u32,

    #[serde(default = "Config::default_watch")]
    pub watch: bool,
}

impl Config {
    pub fn new(config: &::config::Config) -> Option<Self> {
        if let Some(mut config) =
            settings::get_with_default::<Option<config::Config>, _>(config, "static_routes", None)
                .trace_expect("Invalid 'static_routes' section in configuration")
        {
            // Try to create canonical file path
            if let Ok(r) = config.routes_file.canonicalize() {
                config.routes_file = r;
            }

            // Ensure it's absolute
            if config.routes_file.is_relative() {
                let mut path =
                    std::env::current_dir().trace_expect("Failed to get current directory");
                path.push(&config.routes_file);
                config.routes_file = path;
            }
            Some(config)
        } else {
            None
        }
    }

    fn default_path() -> PathBuf {
        settings::config_dir().join("static_routes")
    }

    fn default_priority() -> u32 {
        100
    }

    fn default_watch() -> bool {
        true
    }
}
