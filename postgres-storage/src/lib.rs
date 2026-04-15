/*!
PostgreSQL-backed metadata storage for the Hardy BPA.

This crate implements the [`hardy_bpa::storage::MetadataStorage`] trait using
PostgreSQL as the persistent store.  Bundle metadata is stored as JSON blobs
alongside typed, indexed columns for status, expiry, and keyset-paginated
polling.  Schema migrations are managed by `sqlx::migrate!` and can be
applied automatically on startup or validated against the running database.
*/

mod config;
mod status;
mod storage;

pub use config::Config;

use std::sync::Arc;

/// Errors returned by the PostgreSQL metadata storage layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A configuration value is missing or invalid.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The database contains a migration version not known to this binary,
    /// indicating the schema is newer than the code (downgrade scenario).
    #[error("database has migration version {0} not known to this binary; binary may be too old")]
    Downgrade(i64),
    /// A schema migration failed or a checksum/version mismatch was detected.
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// An underlying `sqlx` database error.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// Create a new PostgreSQL metadata storage instance.
///
/// Connects to the database described by `config`, optionally running pending
/// migrations when `upgrade` is `true`.  When `upgrade` is `false` the schema
/// is validated without modification, failing on any pending or unknown
/// migrations.
pub async fn new(
    config: &Config,
    upgrade: bool,
) -> Result<Arc<dyn hardy_bpa::storage::MetadataStorage>, Error> {
    Ok(Arc::new(storage::Storage::new(config, upgrade).await?))
}
