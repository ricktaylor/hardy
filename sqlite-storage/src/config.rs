use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub db_dir: String,
}

impl Config {
    pub const KEY: &'static str = "sqlite";
}
