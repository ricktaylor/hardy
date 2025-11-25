use super::*;
use hardy_bpv7::eid::Eid;
use rand::Rng;
use trace_err::*;

mod cancel;
mod exec;
mod payload;
mod service;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Flags {
    /// Request reception status reports
    #[value(name = "rcv")]
    Reception,
    /// Request forwarding status reports
    #[value(name = "fwd")]
    Forwarded,
    /// Request delivery status reports
    #[value(name = "dlv")]
    Delivered,
    /// Request deletion status reports
    #[value(name = "del")]
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Verbosity {
    /// Designates very low priority, often extremely verbose, information.
    #[value(name = "trace")]
    Trace,

    /// Designates lower priority information.
    #[value(name = "debug")]
    Debug,

    /// Designates useful information.
    #[value(name = "info")]
    Info,
    /// Designates hazardous situations.
    #[value(name = "warn")]
    Warn,

    /// Designates very serious errors.
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

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// Output additional information, default 'info'.
    #[arg(short, long, num_args = 0..=1, require_equals = true, default_missing_value = "info")]
    verbose: Option<Verbosity>,

    /// The optional lifetime of the bundle, or calculated based on --interval and --wait if not supplied
    #[arg(short, long)]
    lifetime: Option<humantime::Duration>,

    /// The number of bundles to send
    #[arg(short, long)]
    count: Option<u32>,

    /// The time interval to wait between sending bundles
    #[arg(short, long, default_value = "1s")]
    interval: humantime::Duration,

    /// The optional time to wait for responses after sending the last bundle, no value means forever
    #[arg(short, long, num_args = 0..=1, require_equals = true)]
    wait: Option<Option<humantime::Duration>>,

    /// One or more status reporting flags, seperated by ','
    #[arg(short('r'), long, value_delimiter = ',')]
    flags: Vec<Flags>,

    /// The optional "Report To" Endpoint ID (EID) of the bundle
    #[arg(short('R'), long = "report-to")]
    report_to: Option<Eid>,

    /// The source Endpoint ID (EID) of the bundle
    #[arg(short, long)]
    source: Option<Eid>,

    /// The destination Endpoint ID (EID) of the bundle
    destination: Eid,

    /// The CLA address of the next hop.
    address: Option<String>,

    /// Accept self-signed TLS certificates (for testing only, insecure)
    #[arg(long = "tls-accept-self-signed")]
    tls_accept_self_signed: bool,

    /// Path to CA bundle directory (all .crt/.pem files in the directory will be loaded) for TLS certificate validation
    #[arg(long = "tls-ca-bundle")]
    tls_ca_bundle: Option<std::path::PathBuf>,
}

impl Command {
    pub fn lifetime(&self) -> std::time::Duration {
        self.lifetime.map_or_else(
            || {
                if let Some(Some(wait)) = &self.wait
                    && let Some(count) = &self.count
                {
                    let interval: std::time::Duration = self.interval.into();
                    interval.saturating_mul(*count) + **wait
                } else {
                    std::time::Duration::from_secs(86_400)
                }
            },
            |l| l.into(),
        )
    }

    pub fn node_id(&self) -> anyhow::Result<Eid> {
        Ok(match self.source.as_ref().unwrap() {
            Eid::LegacyIpn {
                allocator_id,
                node_number,
                ..
            }
            | Eid::Ipn {
                allocator_id,
                node_number,
                ..
            } => Eid::Ipn {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: 0,
            },
            Eid::Dtn { node_name, .. } => Eid::Dtn {
                node_name: node_name.clone(),
                demux: "".into(),
            },
            eid => {
                return Err(anyhow::anyhow!(
                    "Invalid source EID '{eid}' for ping service"
                ));
            }
        })
    }

    pub fn exec(mut self) -> anyhow::Result<()> {
        if let Some(level) = self.verbose.map(tracing::Level::from) {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(level)
                .with_target(level > tracing::Level::INFO)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|e| anyhow::anyhow!("Failed to set global default subscriber: {e}"))?;
        }

        if self.source.is_none() {
            // Create a random EID
            let mut rng = rand::rng();
            self.source = Some(Eid::Ipn {
                allocator_id: 0,
                node_number: rng.random_range(1..=16383),
                service_number: rng.random_range(1..=127),
            })
        }

        if !self.flags.is_empty() && self.report_to.is_none() {
            self.report_to = self.source.clone();
        }

        exec::exec(self)
    }
}
