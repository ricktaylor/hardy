#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Failed to read configuration: {0}")]
    ConfigRead(#[from] config::ConfigError),
}
