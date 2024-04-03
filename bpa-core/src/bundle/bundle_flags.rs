use super::*;

#[derive(Default)]
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

impl BundleFlags {
    pub fn new(f: u64) -> Self {
        let mut flags = BundleFlags::default();
        for b in 0..=20 {
            if f & (1 << b) != 0 {
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
        if f & !((2 ^ 20) - 1) != 0 {
            log::info!(
                "Parsing bundle primary block with unassigned flag bits set: {:#x}",
                f
            );
        }
        flags
    }

    pub fn as_u64(&self) -> u64 {
        let mut flags: u64 = 0;
        if self.is_fragment {
            flags |= 1 << 0;
        }
        if self.is_admin_record {
            flags |= 1 << 1;
        }
        if self.do_not_fragment {
            flags |= 1 << 2;
        }
        if self.app_ack_requested {
            flags |= 1 << 5;
        }
        if self.report_status_time {
            flags |= 1 << 6;
        }
        if self.receipt_report_requested {
            flags |= 1 << 14;
        }
        if self.forward_report_requested {
            flags |= 1 << 16;
        }
        if self.delivery_report_requested {
            flags |= 1 << 17;
        }
        if self.delete_report_requested {
            flags |= 1 << 18;
        }
        flags
    }
}

impl cbor::decode::FromCbor for BundleFlags {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        let (flags, o, tags) = cbor::decode::parse_detail(data)?;
        Ok((BundleFlags::new(flags), o, tags))
    }
}
