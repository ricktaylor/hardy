use super::*;
use hardy_bpv7::{block, bundle, crc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ArgBlockFlags {
    /// Set all flags
    All,

    /// Clear all flags (default)
    None,

    /// Block must be replicated in every fragment
    #[value(name = "must-replicate", alias = "replicate")]
    MustReplicate,

    /// Generate status report if block processing fails
    #[value(name = "report-on-failure", alias = "report")]
    ReportOnFailure,

    /// Delete bundle if block processing fails
    #[value(name = "delete-bundle-on-failure", alias = "delete-bundle")]
    DeleteBundleOnFailure,

    /// Delete block if processing fails
    #[value(name = "delete-block-on-failure", alias = "delete-block")]
    DeleteBlockOnFailure,
}

impl ArgBlockFlags {
    pub fn to_block_flags(args: &[ArgBlockFlags]) -> block::Flags {
        let mut flags = block::Flags::default();

        for arg in args {
            match arg {
                ArgBlockFlags::All => {
                    flags.must_replicate = true;
                    flags.report_on_failure = true;
                    flags.delete_bundle_on_failure = true;
                    flags.delete_block_on_failure = true;
                }
                ArgBlockFlags::None => {
                    flags = block::Flags::default();
                }
                ArgBlockFlags::MustReplicate => flags.must_replicate = true,
                ArgBlockFlags::ReportOnFailure => flags.report_on_failure = true,
                ArgBlockFlags::DeleteBundleOnFailure => flags.delete_bundle_on_failure = true,
                ArgBlockFlags::DeleteBlockOnFailure => flags.delete_block_on_failure = true,
            }
        }
        flags
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ArgBundleFlags {
    /// Set all flags
    All,

    /// Clear all flags (default)
    None,

    /// Specify ADU is an administrative record
    #[value(name = "admin-record", alias = "admin")]
    IsAdminRecord,

    /// Require bundle to not be fragmented
    #[value(name = "do-not-fragment", alias = "dnf")]
    DoNotFragment,

    /// Application requests acknowledgement
    #[value(name = "ack-requested", alias = "ack")]
    AppAckRequested,

    /// Request status time in status reports
    #[value(name = "report-status-time", alias = "time")]
    ReportStatusTime,

    /// Request reception status reports
    #[value(name = "report-receiption", alias = "rcv")]
    ReceiptReportRequested,

    /// Request forwarding status reports
    #[value(name = "report-forwarding", alias = "fwd")]
    ForwardReportRequested,

    /// Request delivery status reports
    #[value(name = "report-delivery", alias = "dlv")]
    DeliveryReportRequested,

    /// Request deletion status reports
    #[value(name = "report-deletion", alias = "del")]
    DeleteReportRequested,
}

impl ArgBundleFlags {
    pub fn to_bundle_flags(args: &[ArgBundleFlags]) -> Option<bundle::Flags> {
        if args.is_empty() {
            None
        } else {
            let mut flags = bundle::Flags::default();
            for arg in args {
                match arg {
                    ArgBundleFlags::All => {
                        flags.is_admin_record = true;
                        flags.do_not_fragment = true;
                        flags.app_ack_requested = true;
                        flags.report_status_time = true;
                        flags.receipt_report_requested = true;
                        flags.forward_report_requested = true;
                        flags.delivery_report_requested = true;
                        flags.delete_report_requested = true;
                    }
                    ArgBundleFlags::None => {
                        flags = bundle::Flags::default();
                    }
                    ArgBundleFlags::IsAdminRecord => flags.is_admin_record = true,
                    ArgBundleFlags::DoNotFragment => flags.do_not_fragment = true,
                    ArgBundleFlags::AppAckRequested => flags.app_ack_requested = true,
                    ArgBundleFlags::ReportStatusTime => flags.report_status_time = true,
                    ArgBundleFlags::ReceiptReportRequested => flags.receipt_report_requested = true,
                    ArgBundleFlags::ForwardReportRequested => flags.forward_report_requested = true,
                    ArgBundleFlags::DeliveryReportRequested => {
                        flags.delivery_report_requested = true
                    }
                    ArgBundleFlags::DeleteReportRequested => flags.delete_report_requested = true,
                }
            }
            Some(flags)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ArgCrcType {
    /// no Cyclic Redundancy Check (CRC)
    None,

    /// Standard X-25 CRC-16 [aliases: crc16-x25, 16]
    #[clap(alias = "crc16-x25", alias = "16")]
    Crc16,

    /// Standard CRC32C (Castagnoli) CRC-32 [aliases: crc32-castagnoli, 32]
    #[clap(alias = "crc32-castagnoli", alias = "32")]
    Crc32,
}

impl From<ArgCrcType> for crc::CrcType {
    fn from(value: ArgCrcType) -> Self {
        match value {
            ArgCrcType::None => crc::CrcType::None,
            ArgCrcType::Crc16 => crc::CrcType::CRC16_X25,
            ArgCrcType::Crc32 => crc::CrcType::CRC32_CASTAGNOLI,
        }
    }
}
