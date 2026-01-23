use super::*;
use hardy_bpv7::eid::Eid;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// The source Endpoint ID (EID) of the bundle
    #[arg(short, long)]
    source: Eid,

    /// The destination Endpoint ID (EID) of the bundle
    #[arg(short, long)]
    destination: Eid,

    /// The optional 'Report To' Endpoint ID (EID) of the bundle
    #[arg(short, long = "report-to")]
    report_to: Option<Eid>,

    /// Payload from command line
    #[arg(short, long, conflicts_with = "payload_file")]
    payload: Option<String>,

    /// Path to file containing payload, '-' for stdin
    #[arg(long = "payload-file", conflicts_with = "payload")]
    payload_file: Option<io::Input>,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// One or more bundle processing control flags, seperated by ','
    #[arg(short, long, value_delimiter = ',')]
    flags: Vec<flags::ArgBundleFlags>,

    /// The CRC type of the bundle
    #[arg(short, long = "crc-type")]
    crc_type: Option<flags::ArgCrcType>,

    /// The optional lifetime of the bundle, or 24 hours if not supplied
    #[arg(short, long)]
    lifetime: Option<humantime::Duration>,

    /// The optional hop_limit of the bundle.
    #[arg(short('H'), long = "hop-limit")]
    hop_limit: Option<u64>,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        // Get payload data
        let payload_data = if let Some(payload_str) = &self.payload {
            payload_str.as_bytes().to_vec()
        } else if let Some(input) = &self.payload_file {
            input.read_all()?
        } else {
            return Err(anyhow::anyhow!(
                "Either --payload or --payload-file must be provided"
            ));
        };

        let builder: hardy_bpv7::builder::Builder = hardy_bpv7::builder::BundleTemplate {
            source: self.source,
            destination: self.destination,
            report_to: self.report_to,
            flags: flags::ArgBundleFlags::to_bundle_flags(&self.flags),
            crc_type: self.crc_type.map(Into::into),
            lifetime: {
                if let Some(lifetime) = self.lifetime {
                    if lifetime.as_millis() > u64::MAX as u128 {
                        return Err(anyhow::anyhow!("Lifetime too long: {lifetime}!"));
                    }
                    Some(lifetime.into())
                } else {
                    None
                }
            },
            hop_limit: self.hop_limit,
        }
        .into();

        self.output.write_all(
            &builder
                .with_payload(payload_data.into())
                .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
                .map_err(|e| anyhow::anyhow!("Failed to build bundle: {e}"))?
                .1,
        )
    }
}
