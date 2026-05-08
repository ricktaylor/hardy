//! BPA storage subsystem.
//!
//! The backend contract — what an external storage backend implements
//! against — lives in [`backend`] and is re-exported here for convenience.
//! Streaming primitives used by the contract live in [`crate::stream`].
//! Everything else in this module (the in-process `Store`, the dispatcher
//! channel, the reaper, the recover/reassembly helpers) is BPA-internal
//! infrastructure.

pub mod backend;

mod bundle_mem;
mod cached;
mod metadata_mem;
mod reaper;

pub(crate) mod adu_reassembly;
pub(crate) mod channel;
pub(crate) mod recover;
pub(crate) mod store;

// Re-exports

/// In-memory [`BundleStorage`] backend, suitable for testing and ephemeral deployments.
pub use bundle_mem::{BundleMemStorage, Config as BundleMemStorageConfig};
/// In-memory [`MetadataStorage`] backend, suitable for testing and ephemeral deployments.
pub use metadata_mem::{Config as MetadataMemStorageConfig, MetadataMemStorage};

pub use cached::{CachedBundleStorage, DEFAULT_LRU_CAPACITY, DEFAULT_MAX_CACHED_BUNDLE_SIZE};

pub use backend::*;
