use super::*;
use hardy_bpv7::bundle::Flags;
use hardy_bpv7::crc::CrcType;
use hardy_bpv7::eid::Eid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ArgFlags {
    /// Specify bundle is a fragment
    #[value(name = "isfrag")]
    IsFragment = 1 << 0,

    /// Specify ADU is an administrative record
    #[value(name = "isadm")]
    IsAdminRecord = 1 << 1,

    /// Require bundle to not be fragmented
    #[value(name = "nofrag")]
    DoNotFragment = 1 << 2,

    /// Request acknowledgement by application
    #[value(name = "ack")]
    AppAckRequested = 1 << 5,

    /// Request status time in status reports
    #[value(name = "time")]
    ReportStatusTime = 1 << 6,

    /// Request reception status reports
    #[value(name = "rcv")]
    ReceiptReportRequested = 1 << 14,

    /// Request forwarding status reports
    #[value(name = "fwd")]
    ForwardReportRequested = 1 << 16,

    /// Request delivery status reports
    #[value(name = "dlv")]
    DeliveryReportRequested = 1 << 17,

    /// Request deletion status reports
    #[value(name = "del")]
    DeleteReportRequested = 1 << 18,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ArgCrcType {
    /// no Cyclic Redundancy Check (CRC)
    #[clap(alias = "none")]
    None,

    /// Standard X-25 CRC-16 [aliases: crc16_x25, 16]
    #[clap(alias = "crc16_x25", alias = "16")]
    Crc16,

    /// Standard CRC32C (Castagnoli) CRC-32 [aliases: crc32_castagnoli, 32]
    #[clap(alias = "crc32_castagnoli", alias = "32")]
    Crc32,
}

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

    /// The file to use as payload, use '-' for stdin
    #[arg(short, long)]
    payload: io::Input,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// One or more bundle processing control flags, seperated by ','
    #[arg(short, long, value_delimiter = ',')]
    flags: Vec<ArgFlags>,

    /// The CRC type of the bundle
    #[arg(short, long = "crc-type")]
    crc_type: Option<ArgCrcType>,

    /// The optional lifetime of the bundle, or 24 hours if not supplied
    #[arg(short, long)]
    lifetime: Option<humantime::Duration>,

    /// The optional hop_limit of the bundle.
    #[arg(short('H'), long = "hop-limit")]
    hop_limit: Option<u64>,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        // Accumulate bundle processing control flags
        let flags: Option<Flags> = if self.flags.is_empty() {
            None
        } else {
            Some(Flags::from(
                self.flags.iter().map(|flag| *flag as u64).sum::<u64>(),
            ))
        };

        let crc_val = self.crc_type.map(|crc| crc as u64);

        let builder: hardy_bpv7::builder::Builder = hardy_bpv7::builder::BundleTemplate {
            source: self.source,
            destination: self.destination,
            report_to: self.report_to,
            flags,
            crc_type: crc_val.map(CrcType::from),
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
                .with_payload(self.payload.read_all()?.into())
                .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
                .map_err(|e| anyhow::anyhow!("Failed to build bundle: {e}"))?
                .1,
        )
    }
}
