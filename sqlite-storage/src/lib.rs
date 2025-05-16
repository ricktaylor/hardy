mod config;
mod migrate;
mod storage;

pub use config::Config;

use trace_err::*;
use tracing::*;

pub fn new(
    config: &config::Config,
    upgrade: bool,
) -> std::sync::Arc<dyn hardy_bpa::storage::MetadataStorage> {
    // Ensure directory exists
    std::fs::create_dir_all(&config.db_dir).trace_expect(&format!(
        "Failed to create metadata store directory {}",
        config.db_dir.display()
    ));

    std::sync::Arc::new(storage::Storage::new(config, upgrade))
}
