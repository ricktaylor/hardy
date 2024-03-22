use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub cache_dir: String,
}

impl Config {
    pub const KEY: &'static str = "localdisk";
}
