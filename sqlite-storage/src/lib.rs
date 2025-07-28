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
    std::sync::Arc::new(storage::Storage::new(config, upgrade))
}
