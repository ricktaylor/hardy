//! SQLite-backed metadata storage for the Hardy BPA.
//!
//! This crate provides a persistent [`MetadataStorage`](hardy_bpa::storage::MetadataStorage)
//! implementation that stores bundle metadata in a SQLite database. It handles
//! schema migrations, connection pooling, serialized write access, and the
//! startup recovery protocol (mark-unconfirmed / confirm / sweep).
//!
//! # Key types
//!
//! - [`Config`] -- database directory and filename settings (serde-deserializable).
//! - [`new()`] -- constructs an `Arc<dyn MetadataStorage>` ready for use by the BPA.

mod config;
mod migrate;
mod storage;

pub use config::Config;

use trace_err::*;
use tracing::{error, info, warn};

#[cfg(feature = "instrument")]
use tracing::instrument;

use rusqlite::OptionalExtension;

/// Creates a new SQLite metadata storage instance.
///
/// Opens (or creates) the database specified by `config`, runs schema migrations
/// when `upgrade` is `true`, and returns the storage behind an `Arc<dyn MetadataStorage>`.
pub fn new(
    config: &config::Config,
    upgrade: bool,
) -> std::sync::Arc<dyn hardy_bpa::storage::MetadataStorage> {
    std::sync::Arc::new(storage::Storage::new(config, upgrade))
}
