/*!
PostgreSQL-backed metadata storage for the Hardy BPA.

This crate implements the [`hardy_bpa::storage::MetadataStorage`] trait using
PostgreSQL as the persistent store.  Bundle metadata is stored as JSON blobs
alongside typed, indexed columns for status, expiry, and keyset-paginated
polling.  Schema migrations are managed by `sqlx::migrate!` and can be
applied automatically on startup or validated against the running database.
*/

mod builder;
mod status;
mod storage;

pub use self::builder::PostgresStorageBuilder;
pub use self::storage::PostgresStorage;

/// Errors returned by the PostgreSQL metadata storage layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No database URL was configured.
    #[error(
        "no database URL configured: chain `database_url()` or set the DATABASE_URL environment variable"
    )]
    NoDatabaseUrl,
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
