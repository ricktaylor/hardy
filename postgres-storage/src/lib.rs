mod config;
mod status;
mod storage;

pub use config::Config;

use std::sync::Arc;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub async fn new(
    config: &Config,
    upgrade: bool,
) -> Result<Arc<dyn hardy_bpa::storage::MetadataStorage>, Error> {
    Ok(Arc::new(storage::Storage::new(config, upgrade).await?))
}
