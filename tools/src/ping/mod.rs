use super::*;
use hardy_bpv7::eid::Eid;
use rand::Rng;
use trace_err::*;

mod cancel;
mod exec;
mod payload;
mod service;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    /// Use text format
    #[value(name = "text")]
    Text,
    /// Use binary format
    #[value(name = "binary")]
    Binary,
}

fn parse_flags(s: &str) -> anyhow::Result<hardy_bpa::service::SendFlags> {
    let mut flags = hardy_bpa::service::SendFlags::default();
    for flag in s.split(',') {
        match flag {
            "rcv" => {
                flags.report_status_time = true;
                flags.notify_reception = true;
            }
            "ct" => eprintln!("Ignoring 'ct' flag"),
            "ctr" => eprintln!("Ignoring 'ctr' flag"),
            "fwd" => {
                flags.report_status_time = true;
                flags.notify_forwarding = true;
            }
            "dlv" => {
                flags.report_status_time = true;
                flags.notify_delivery = true;
            }
            "del" => {
                flags.report_status_time = true;
                flags.notify_deletion = true;
            }
            _ => return Err(anyhow::anyhow!("invalid flag: {}", flag)),
        }
    }
    Ok(flags)
}

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// Verbosity level
    #[arg(short, long, num_args = 0..=1)]
    verbose: Option<Option<tracing::Level>>,

    /// The optional lifetime of the bundle, or 24 hours if not supplied
    #[arg(short, long)]
    lifetime: Option<humantime::Duration>,

    /// The Time-to-Live (in seconds) for the bundles
    #[arg(short, long)]
    ttl: Option<u64>,

    /// The number of bundles to send
    #[arg(short, long)]
    count: Option<u32>,

    /// The time interval (in seconds) to wait between sending bundles
    #[arg(short, long, default_value = "1")]
    interval: u64,

    /// The priority of the bundles (ignored)
    #[arg(short, long)]
    priority: Option<i32>,

    /// The time (in seconds) to wait for responses after sending the last bundle, -1 means forever
    #[arg(short, long, default_value = "10")]
    wait: i64,

    /// Status reporting flags, can be any combination of rcv,dnf,fwd,dlv,del delimited by ',' (without spaces)
    #[arg(short('r'), long, value_parser = parse_flags)]
    flags: Option<hardy_bpa::service::SendFlags>,

    /// The optional 'Report To' Endpoint ID (EID) of the bundle
    #[arg(short('R'), long = "report-to")]
    report_to: Option<Eid>,

    /// Set the output format
    #[arg(short, long, default_value = "text")]
    format: Format,

    /// The source Endpoint ID (EID) of the bundle
    #[arg(short, long)]
    source: Option<Eid>,

    /// The destination Endpoint ID (EID) of the bundle
    destination: Eid,

    /// The CLA address of the next hop.
    address: Option<String>,
}

impl Command {
    pub fn lifetime(&self) -> std::time::Duration {
        self.lifetime
            .map(|l| l.into())
            .or_else(|| self.ttl.map(std::time::Duration::from_secs))
            .unwrap_or(std::time::Duration::from_hours(1))
    }

    pub fn exec(mut self) -> anyhow::Result<()> {
        if let Some(level) = self.verbose.map(|o| o.unwrap_or(tracing::Level::INFO)) {
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

        if self.flags.is_some() && self.report_to.is_none() {
            self.report_to = self.source.clone();
        }

        exec::exec(self)
    }
}
