#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// PostgreSQL connection string. Falls back to the DATABASE_URL env var.
    pub database_url: String,
    /// Maximum number of pooled connections.
    pub max_connections: u32,
    /// Minimum number of idle connections kept alive.
    pub min_connections: u32,
    /// Seconds to wait when acquiring a connection before erroring.
    pub connect_timeout_secs: u64,
    /// Minutes before an idle connection is closed.
    pub idle_timeout_mins: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
            max_connections: 10,
            min_connections: 1,
            connect_timeout_secs: 30,
            idle_timeout_mins: 10,
        }
    }
}
