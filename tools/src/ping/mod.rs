use super::*;
use hardy_bpv7::eid::Eid;
use rand::Rng;

mod exec;
mod payload;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    /// Use text format
    #[value(name = "text")]
    Text,
    /// Use binary format
    #[value(name = "binary")]
    Binary,
}

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// The optional lifetime of the bundle, or 24 hours if not supplied
    #[arg(short, long)]
    lifetime: Option<humantime::Duration>,

    /// The Time-to-Live (in seconds) for the bundles
    #[arg(short, long)]
    ttl: Option<u64>,

    /// The number of bundles to send
    #[arg(short, long)]
    count: u32,

    /// The time interval (in seconds) to wait between sending bundles
    #[arg(short, long)]
    interval: u64,

    /// The priority of the bundles (ignored)
    #[arg(short, long)]
    priority: i32,

    /// The time (in seconds) to wait for responses after sending the last bundle
    #[arg(short, long)]
    wait: u64,

    /// The source Endpoint ID (EID) of the bundle
    #[arg(short, long)]
    source: Option<Eid>,

    /// The destination Endpoint ID (EID) of the bundle
    #[arg(short, long)]
    destination: Eid,

    /// The optional 'Report To' Endpoint ID (EID) of the bundle
    #[arg(short, long = "report-to")]
    report_to: Option<Eid>,

    /// The CLA address of the next hop.
    #[arg(short, long)]
    address: String,

    /// Set the output format
    #[arg(short, long, default_value = "text")]
    format: Format,
}

impl Command {
    pub fn lifetime(&self) -> Option<std::time::Duration> {
        self.lifetime
            .map(|l| l.into())
            .or_else(|| self.ttl.map(std::time::Duration::from_secs))
    }

    pub fn exec(mut self) -> anyhow::Result<()> {
        if self.source.is_none() {
            // Create a random EID
            let mut rng = rand::rng();
            self.source = Some(Eid::Ipn {
                allocator_id: 0,
                node_number: rng.random_range(1..=16383),
                service_number: rng.random_range(1..=127),
            })
        }

        exec::exec(self)
    }
}
