/*!
Local-disk bundle storage backend for Hardy BPA.

Implements the [`hardy_bpa::storage::BundleStorage`] trait using the local filesystem.
Bundles are stored as individual files distributed across a two-level hexadecimal
directory structure (`xx/yy/`) to avoid filesystem bottlenecks from large flat directories.
An optional `fsync` mode provides crash-safe atomic writes via temp-file-and-rename.

# Key types

- [`Config`] — Storage configuration (directory path, fsync toggle).
- [`new`] — Constructor that creates the store directory and returns a `BundleStorage` handle.
*/

mod config;
mod storage;

pub use config::Config;

use trace_err::*;
use tracing::{error, info, warn};

#[cfg(feature = "instrument")]
use tracing::instrument;

/// Creates a new local-disk bundle storage instance.
///
/// Ensures the configured store directory exists (creating it if necessary)
/// and returns an `Arc<dyn BundleStorage>` ready for use by the BPA.
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
