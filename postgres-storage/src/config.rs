/// Configuration for the PostgreSQL metadata storage backend.
///
/// All fields use `#[serde(default)]` so only overrides need to be specified
/// in the TOML config.  Field names are kebab-case in config files
/// (e.g. `max-connections`).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// PostgreSQL connection string (e.g. `postgres://user:pass@host/db`).
    /// Falls back to the `DATABASE_URL` environment variable when empty.
    /// Default: `""` (empty -- `DATABASE_URL` env var must be set).
    pub database_url: String,
    /// Maximum number of pooled connections.
    /// For scale deployments this should be sized to `worker_threads * 2` or higher;
    /// the default of 20 suits moderate throughput with a single node.
    /// Default: `20`.
    pub max_connections: u32,
    /// Minimum number of idle connections kept alive in the pool.
    /// Default: `2`.
    pub min_connections: u32,
    /// Seconds to wait when acquiring a connection before returning an error.
    /// Default: `30`.
    pub connect_timeout_secs: u64,
    /// Minutes before an idle connection is closed and removed from the pool.
    /// Default: `10`.
    pub idle_timeout_mins: u64,
    /// Maximum lifetime of a pooled connection in minutes.
    /// Prevents stale connections after server-side TCP timeouts or firewall resets.
    /// Default: `30`.
    pub max_lifetime_mins: u64,
    /// Number of rows fetched per page in keyset-paginated poll queries.
    /// Larger values reduce round-trips; smaller values reduce per-query memory.
    /// Must be at least 1.  Default: `64`.
    pub poll_page_size: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            max_connections: 20,
            min_connections: 2,
            connect_timeout_secs: 30,
            idle_timeout_mins: 10,
            max_lifetime_mins: 30,
            poll_page_size: 64,
        }
    }
}
