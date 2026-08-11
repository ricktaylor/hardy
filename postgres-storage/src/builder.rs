//! Builder for [`PostgresStorage`].

use core::num::NonZeroU32;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;

use super::{Error, PostgresStorage};

// Pool defaults: 20 connections suits moderate throughput with a single
// node (scale deployments should size to `worker_threads * 2` or higher),
// and the lifetime cap prevents stale connections after server-side TCP
// timeouts or firewall resets.
const DEFAULT_MAX_CONNECTIONS: NonZeroU32 = NonZeroU32::new(20).unwrap();
const DEFAULT_MIN_CONNECTIONS: u32 = 2;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);

// Rows fetched per page in keyset-paginated poll queries: larger values
// reduce round-trips, smaller values reduce per-query memory.
const DEFAULT_POLL_PAGE_SIZE: NonZeroU32 = NonZeroU32::new(64).unwrap();

/// Builder for [`PostgresStorage`], obtained from
/// [`PostgresStorage::builder()`]. Unset knobs apply the backend's own
/// defaults.
#[must_use = "a PostgresStorageBuilder does nothing unless `build()` is called"]
pub struct PostgresStorageBuilder {
    database_url: Option<String>,
    max_connections: Option<NonZeroU32>,
    min_connections: Option<u32>,
    connect_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    max_lifetime: Option<Duration>,
    poll_page_size: Option<NonZeroU32>,
}

impl PostgresStorageBuilder {
    pub(crate) fn new() -> Self {
        Self {
            database_url: None,
            max_connections: None,
            min_connections: None,
            connect_timeout: None,
            idle_timeout: None,
            max_lifetime: None,
            poll_page_size: None,
        }
    }

    /// Sets the PostgreSQL connection string (e.g.
    /// `postgres://user:pass@host/db`); unset falls back to the
    /// `DATABASE_URL` environment variable.
    pub fn database_url(mut self, url: impl Into<String>) -> Self {
        self.database_url = Some(url.into());
        self
    }

    /// Sets the maximum number of pooled connections.
    pub fn max_connections(mut self, limit: NonZeroU32) -> Self {
        self.max_connections = Some(limit);
        self
    }

    /// Sets the minimum number of idle connections kept alive in the pool.
    pub fn min_connections(mut self, limit: u32) -> Self {
        self.min_connections = Some(limit);
        self
    }

    /// Sets how long to wait when acquiring a connection before returning
    /// an error.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Sets how long a connection may sit idle before it is closed and
    /// removed from the pool.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = Some(timeout);
        self
    }

    /// Sets the maximum lifetime of a pooled connection.
    pub fn max_lifetime(mut self, lifetime: Duration) -> Self {
        self.max_lifetime = Some(lifetime);
        self
    }

    /// Sets the number of rows fetched per page in keyset-paginated poll
    /// queries.
    pub fn poll_page_size(mut self, size: NonZeroU32) -> Self {
        self.poll_page_size = Some(size);
        self
    }

    /// Connects to the database, optionally running pending migrations
    /// when `upgrade` is `true`. When `upgrade` is `false` the schema is
    /// validated without modification, failing on any pending or unknown
    /// migrations.
    ///
    /// # Errors
    ///
    /// Fails if no database URL is configured (neither chained nor in the
    /// `DATABASE_URL` environment variable), if the connection cannot be
    /// established, or on any migration mismatch.
    pub async fn build(self, upgrade: bool) -> Result<PostgresStorage, Error> {
        let database_url = self
            .database_url
            .or_else(|| std::env::var("DATABASE_URL").ok())
            .filter(|url| !url.is_empty())
            .ok_or(Error::NoDatabaseUrl)?;

        let pool_options = PgPoolOptions::new()
            .max_connections(
                self.max_connections
                    .unwrap_or(DEFAULT_MAX_CONNECTIONS)
                    .get(),
            )
            .min_connections(self.min_connections.unwrap_or(DEFAULT_MIN_CONNECTIONS))
            .acquire_timeout(self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT))
            .idle_timeout(self.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT))
            .max_lifetime(self.max_lifetime.unwrap_or(DEFAULT_MAX_LIFETIME));

        PostgresStorage::new(
            pool_options,
            &database_url,
            self.poll_page_size.unwrap_or(DEFAULT_POLL_PAGE_SIZE),
            upgrade,
        )
        .await
    }
}
