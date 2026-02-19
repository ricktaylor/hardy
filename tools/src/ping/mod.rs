use super::*;
use hardy_bpv7::eid::{Eid, NodeId};
use rand::RngExt;
use trace_err::*;

mod cancel;
mod exec;
mod payload;
mod service;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Verbosity {
    /// Most verbose, all internal details
    #[value(name = "trace")]
    Trace,

    /// Debug information
    #[value(name = "debug")]
    Debug,

    /// Informational messages
    #[value(name = "info")]
    Info,

    /// Warnings only
    #[value(name = "warn")]
    Warn,

    /// Errors only
    #[value(name = "error")]
    Error,
}

impl From<Verbosity> for tracing::Level {
    fn from(value: Verbosity) -> Self {
        match value {
            Verbosity::Trace => tracing::Level::TRACE,
            Verbosity::Debug => tracing::Level::DEBUG,
            Verbosity::Info => tracing::Level::INFO,
            Verbosity::Warn => tracing::Level::WARN,
            Verbosity::Error => tracing::Level::ERROR,
        }
    }
}

/// Send ping bundles to a destination endpoint and measure round-trip times.
///
/// Embeds a minimal BPA and establishes a direct TCPCLv4 connection. Bundles are
/// signed by default to detect corruption. Press Ctrl+C to stop and show statistics.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// Number of pings to send
    #[arg(short, long)]
    count: Option<u32>,

    /// Interval between pings
    #[arg(short, long, default_value = "1s")]
    interval: humantime::Duration,

    /// Target bundle size in bytes (for MTU testing)
    #[arg(short, long)]
    size: Option<usize>,

    /// Total time limit for the session
    #[arg(short = 'w', long)]
    timeout: Option<humantime::Duration>,

    /// Time to wait for responses after last ping
    #[arg(short = 'W', long)]
    wait: Option<humantime::Duration>,

    /// Only show summary statistics
    #[arg(short, long)]
    quiet: bool,

    /// Verbose output [trace, debug, info, warn, error]
    #[arg(short, long, num_args = 0..=1, require_equals = true, default_missing_value = "info")]
    verbose: Option<Verbosity>,

    /// Hop limit (like IP TTL)
    #[arg(short = 't', long)]
    ttl: Option<u64>,

    /// Bundle lifetime
    #[arg(long)]
    lifetime: Option<humantime::Duration>,

    /// Disable BIB signing
    #[arg(long)]
    no_sign: bool,

    /// Source EID
    #[arg(short = 'S', long)]
    source: Option<Eid>,

    /// Destination EID to ping
    destination: Eid,

    /// TCPCLv4 peer address (host:port)
    peer: Option<String>,

    /// Accept self-signed TLS certificates
    #[arg(long = "tls-insecure")]
    tls_insecure: bool,

    /// CA bundle directory for TLS
    #[arg(long = "tls-ca")]
    tls_ca: Option<std::path::PathBuf>,
}

impl Command {
    pub fn lifetime(&self) -> std::time::Duration {
        self.lifetime.map_or_else(
            || {
                // Calculate lifetime from session parameters if not explicitly specified
                let interval: std::time::Duration = self.interval.into();

                // Wait time after sending (default to one interval if not specified)
                let wait_time = self.wait.map(|w| *w).unwrap_or(interval);

                if let Some(count) = self.count {
                    // Finite mode: lifetime = time to send all + wait time
                    interval.saturating_mul(count) + wait_time
                } else {
                    // Infinite mode: use timeout if set, otherwise default to 5 minutes
                    // This covers the common case of Ctrl+C'ing after a few pings
                    self.timeout
                        .map(|t| *t)
                        .unwrap_or(std::time::Duration::from_secs(300))
                }
            },
            |l| l.into(),
        )
    }

    pub fn node_id(&self) -> anyhow::Result<NodeId> {
        self.source.clone().unwrap().try_to_node_id().map_err(|_| {
            anyhow::anyhow!(
                "Invalid source EID '{}' for ping service",
                self.source.as_ref().unwrap()
            )
        })
    }

    pub fn exec(mut self) -> ! {
        if let Some(level) = self.verbose.map(tracing::Level::from) {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(level)
                .with_target(level > tracing::Level::INFO)
                .finish();
            if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
                eprintln!("Failed to set global default subscriber: {e}");
                std::process::exit(exec::ExitCode::Error as i32);
            }
        }

        if self.source.is_none() {
            // Create a random EID
            let mut rng = rand::rng();
            self.source = Some(Eid::Ipn {
                fqnn: hardy_bpv7::eid::IpnNodeId {
                    allocator_id: 0,
                    node_number: rng.random_range(1..=16383),
                },
                service_number: rng.random_range(1..=127),
            })
        }

        exec::exec(self)
    }
}
