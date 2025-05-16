use trace_err::*;
use tracing::*;

mod config;
mod storage;

pub use config::Config;

pub fn new(
    config: &Config,
    upgrade: bool,
) -> std::sync::Arc<dyn hardy_bpa::storage::BundleStorage> {
    info!(
        "Using bundle store directory: {}",
        config.store_dir.display()
    );

    // Ensure directory exists
    std::fs::create_dir_all(&config.store_dir).trace_expect(&format!(
        "Failed to create bundle store directory {}",
        config.store_dir.display()
    ));

    std::sync::Arc::new(storage::Storage::new(config, upgrade))
}
