/*!
The bundle processing control flags (RFC 9171 Section 4.2.3) and their
wire bit mapping.
*/

use hardy_cbor::{
    decode::FromCbor,
    encode::{Encoder, ToCbor},
};

use crate::{Error, canonical::parse_canonical};

/// Represents the processing control flags for a BPv7 bundle.
///
/// These flags, defined in RFC 9171 Section 4.2.3, control how a node should
/// handle the bundle, such as whether it can be fragmented or if status reports
/// are requested.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BundleFlags {
    /// If set, this bundle is a fragment of a larger bundle.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub is_fragment: bool,

    /// If set, the payload is an administrative record.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub is_admin_record: bool,

    /// If set, the bundle must not be fragmented.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub do_not_fragment: bool,

    /// If set, the destination application is requested to send an acknowledgement.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub app_ack_requested: bool,

    /// If set, status reports should include the time of the reported event.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub report_status_time: bool,

    /// If set, a status report should be generated upon bundle reception.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub receipt_report_requested: bool,

    /// If set, a status report should be generated upon bundle forwarding.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub forward_report_requested: bool,

    /// If set, a status report should be generated upon bundle delivery.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delivery_report_requested: bool,

    /// If set, a status report should be generated upon bundle deletion.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "<&bool as core::ops::Not>::not")
    )]
    pub delete_report_requested: bool,

    /// A bitmask of any unrecognized flags encountered during parsing.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub unrecognised: Option<u64>,
}

impl From<u64> for BundleFlags {
    fn from(value: u64) -> Self {
        let mut flags = Self::default();
        let mut unrecognised = value;

        if (value & (1 << 0)) != 0 {
            flags.is_fragment = true;
            unrecognised &= !(1 << 0);
        }
        if (value & (1 << 1)) != 0 {
            flags.is_admin_record = true;
            unrecognised &= !(1 << 1);
        }
        if (value & (1 << 2)) != 0 {
            flags.do_not_fragment = true;
            unrecognised &= !(1 << 2);
        }
        if (value & (1 << 5)) != 0 {
            flags.app_ack_requested = true;
            unrecognised &= !(1 << 5);
        }
        if (value & (1 << 6)) != 0 {
            flags.report_status_time = true;
            unrecognised &= !(1 << 6);
        }
        if (value & (1 << 14)) != 0 {
            flags.receipt_report_requested = true;
            unrecognised &= !(1 << 14);
        }
        if (value & (1 << 16)) != 0 {
            flags.forward_report_requested = true;
            unrecognised &= !(1 << 16);
        }
        if (value & (1 << 17)) != 0 {
            flags.delivery_report_requested = true;
            unrecognised &= !(1 << 17);
        }
        if (value & (1 << 18)) != 0 {
            flags.delete_report_requested = true;
            unrecognised &= !(1 << 18);
        }

        if unrecognised != 0 {
            flags.unrecognised = Some(unrecognised);
        }
        flags
    }
}

impl From<&BundleFlags> for u64 {
    fn from(value: &BundleFlags) -> Self {
        let mut flags = value.unrecognised.unwrap_or(0);
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

impl ToCbor for BundleFlags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(&u64::from(self))
    }
}

impl FromCbor for BundleFlags {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> core::result::Result<(Self, bool, usize), Self::Error> {
        let (value, len) = parse_canonical::<u64, Error>(data, Error::NotCanonical)?;
        Ok((value.into(), true, len))
    }
}
