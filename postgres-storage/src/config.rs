#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// PostgreSQL connection string. Falls back to the DATABASE_URL env var.
    pub database_url: String,
    /// Maximum number of pooled connections.
    /// For scale deployments this should be sized to `worker_threads * 2` or higher;
    /// the default of 20 suits moderate throughput with a single node.
    pub max_connections: u32,
    /// Minimum number of idle connections kept alive.
    pub min_connections: u32,
    /// Seconds to wait when acquiring a connection before erroring.
    pub connect_timeout_secs: u64,
    /// Minutes before an idle connection is closed.
    pub idle_timeout_mins: u64,
    /// Number of rows fetched per page in keyset-paginated poll queries.
    /// Larger values reduce round-trips; smaller values reduce per-query memory.
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
            poll_page_size: 64,
        }
    }
}
