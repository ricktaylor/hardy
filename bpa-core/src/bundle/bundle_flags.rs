use super::*;

#[derive(Default, Copy, Clone)]
pub struct BundleFlags {
    pub is_fragment: bool,
    pub is_admin_record: bool,
    pub do_not_fragment: bool,
    pub app_ack_requested: bool,
    pub report_status_time: bool,
    pub receipt_report_requested: bool,
    pub forward_report_requested: bool,
    pub delivery_report_requested: bool,
    pub delete_report_requested: bool,
}

impl From<u64> for BundleFlags {
    fn from(value: u64) -> Self {
        let mut flags = BundleFlags::default();
        for b in 0..=20 {
            if value & (1 << b) != 0 {
                match b {
                    0 => flags.is_fragment = true,
                    1 => flags.is_admin_record = true,
                    2 => flags.do_not_fragment = true,
                    5 => flags.app_ack_requested = true,
                    6 => flags.report_status_time = true,
                    14 => {
                        if flags.is_admin_record {
                            log::info!("Parsing bundle primary block with Administrative Record and Receipt Report Requested flag set!");
                        } else {
                            flags.receipt_report_requested = true;
                        }
                    }
                    16 => {
                        if flags.is_admin_record {
                            log::info!("Parsing bundle primary block with Administrative Record and Forward Report Requested flag set!");
                        } else {
                            flags.forward_report_requested = true;
                        }
                    }
                    17 => {
                        if flags.is_admin_record {
                            log::info!("Parsing bundle primary block with Administrative Record and Delivery Report Requested flag set!");
                        } else {
                            flags.delivery_report_requested = true;
                        }
                    }
                    18 => {
                        if flags.is_admin_record {
                            log::info!("Parsing bundle primary block with Administrative Record and Delete Report Requested flag set!");
                        } else {
                            flags.delete_report_requested = true;
                        }
                    }
                    b => log::info!(
                        "Parsing bundle primary block with reserved flag bit {} set",
                        b
                    ),
                }
            }
        }
        if value & !((2 ^ 20) - 1) != 0 {
            log::info!(
                "Parsing bundle primary block with unassigned flag bits set: {:#x}",
                value
            );
        }
        flags
    }
}

impl From<BundleFlags> for u64 {
    fn from(value: BundleFlags) -> Self {
        let mut flags: u64 = 0;
        if value.is_fragment {
            flags |= 1 << 0;
        }
        if value.is_admin_record {
            flags |= 1 << 1;
        }
        if value.do_not_fragment {
            flags |= 1 << 2;
        }
        if value.app_ack_requested {
            flags |= 1 << 5;
        }
        if value.report_status_time {
            flags |= 1 << 6;
        }
        if value.receipt_report_requested {
            flags |= 1 << 14;
        }
        if value.forward_report_requested {
            flags |= 1 << 16;
        }
        if value.delivery_report_requested {
            flags |= 1 << 17;
        }
        if value.delete_report_requested {
            flags |= 1 << 18;
        }
        flags
    }
}

impl cbor::encode::ToCbor for BundleFlags {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        cbor::encode::emit_with_tags::<u64>(self.into(), tags)
    }
}

impl cbor::decode::FromCbor for BundleFlags {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        cbor::decode::parse_detail::<u64>(data).map(|(v, len, tags)| (v.into(), len, tags))
    }
}
