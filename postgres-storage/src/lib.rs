mod config;
mod status;
mod storage;

pub use config::Config;

use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

pub async fn new(
    config: &Config,
    upgrade: bool,
) -> Result<Arc<dyn hardy_bpa::storage::MetadataStorage>, Error> {
    Ok(Arc::new(storage::Storage::new(config, upgrade).await?))
}
