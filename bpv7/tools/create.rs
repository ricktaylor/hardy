use super::*;
use hardy_bpv7::{bundle::Flags, crc::CrcType, eid::Eid};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ArgFlags {
    /// Specify ADU is an administrative record
    #[value(name = "isadm")]
    IsAdminRecord,

    /// Require bundle to not be fragmented
    #[value(name = "nofrag")]
    DoNotFragment,

    /// Request acknowledgement by application
    #[value(name = "ack")]
    AppAckRequested,

    /// Request status time in status reports
    #[value(name = "time")]
    ReportStatusTime,

    /// Request reception status reports
    #[value(name = "rcv")]
    ReceiptReportRequested,

    /// Request forwarding status reports
    #[value(name = "fwd")]
    ForwardReportRequested,

    /// Request delivery status reports
    #[value(name = "dlv")]
    DeliveryReportRequested,

    /// Request deletion status reports
    #[value(name = "del")]
    DeleteReportRequested,
}

impl ArgFlags {
    fn to_bundle_flags(args: &[ArgFlags]) -> Option<Flags> {
        if args.is_empty() {
            None
        } else {
            let mut flags = Flags::default();
            for arg in args {
                match arg {
                    ArgFlags::IsAdminRecord => flags.is_admin_record = true,
                    ArgFlags::DoNotFragment => flags.do_not_fragment = true,
                    ArgFlags::AppAckRequested => flags.app_ack_requested = true,
                    ArgFlags::ReportStatusTime => flags.report_status_time = true,
                    ArgFlags::ReceiptReportRequested => flags.receipt_report_requested = true,
                    ArgFlags::ForwardReportRequested => flags.forward_report_requested = true,
                    ArgFlags::DeliveryReportRequested => flags.delivery_report_requested = true,
                    ArgFlags::DeleteReportRequested => flags.delete_report_requested = true,
                }
            }
            Some(flags)
        }
    }
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

impl From<ArgCrcType> for CrcType {
    fn from(value: ArgCrcType) -> Self {
        match value {
            ArgCrcType::None => CrcType::None,
            ArgCrcType::Crc16 => CrcType::CRC16_X25,
            ArgCrcType::Crc32 => CrcType::CRC32_CASTAGNOLI,
        }
    }
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

    /// Path to the location of the file to use as payload, use '-' for stdin
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
        let builder: hardy_bpv7::builder::Builder = hardy_bpv7::builder::BundleTemplate {
            source: self.source,
            destination: self.destination,
            report_to: self.report_to,
            flags: ArgFlags::to_bundle_flags(&self.flags),
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
                .with_payload(self.payload.read_all()?.into())
                .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
                .map_err(|e| anyhow::anyhow!("Failed to build bundle: {e}"))?
                .1,
        )
    }
}
