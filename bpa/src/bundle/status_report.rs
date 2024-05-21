use super::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Unknown administrative record type {0}")]
    UnknownAdminRecordType(u64),

    #[error("Reserved Status Report Reason Code (255)")]
    ReservedStatusReportReason,

    #[error("Additional items found in array")]
    AdditionalItems,

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("Expecting CBOR array")]
    ArrayExpected(#[from] cbor::decode::Error),
}

trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, Error>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, Error> {
        self.map_err(|e| Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusReportReasonCode {
    NoAdditionalInformation,
    LifetimeExpired,
    ForwardedOverUnidirectionalLink,
    TransmissionCanceled,
    DepletedStorage,
    DestinationEndpointIDUnavailable,
    NoKnownRouteToDestinationFromHere,
    NoTimelyContactWithNextNodeOnRoute,
    BlockUnintelligible,
    HopLimitExceeded,
    TrafficPared,
    BlockUnsupported,
    MissingSecurityOperation,
    UnknownSecurityOperation,
    UnexpectedSecurityOperation,
    FailedSecurityOperation,
    ConflictingSecurityOperation,
    Unassigned(u64),
}

impl Default for StatusReportReasonCode {
    fn default() -> Self {
        Self::NoAdditionalInformation
    }
}

impl From<StatusReportReasonCode> for u64 {
    fn from(value: StatusReportReasonCode) -> Self {
        match value {
            StatusReportReasonCode::NoAdditionalInformation => 0,
            StatusReportReasonCode::LifetimeExpired => 1,
            StatusReportReasonCode::ForwardedOverUnidirectionalLink => 2,
            StatusReportReasonCode::TransmissionCanceled => 3,
            StatusReportReasonCode::DepletedStorage => 4,
            StatusReportReasonCode::DestinationEndpointIDUnavailable => 5,
            StatusReportReasonCode::NoKnownRouteToDestinationFromHere => 6,
            StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute => 7,
            StatusReportReasonCode::BlockUnintelligible => 8,
            StatusReportReasonCode::HopLimitExceeded => 9,
            StatusReportReasonCode::TrafficPared => 10,
            StatusReportReasonCode::BlockUnsupported => 11,
            StatusReportReasonCode::MissingSecurityOperation => 12,
            StatusReportReasonCode::UnknownSecurityOperation => 13,
            StatusReportReasonCode::UnexpectedSecurityOperation => 14,
            StatusReportReasonCode::FailedSecurityOperation => 15,
            StatusReportReasonCode::ConflictingSecurityOperation => 16,
            StatusReportReasonCode::Unassigned(v) => v,
        }
    }
}

impl TryFrom<u64> for StatusReportReasonCode {
    type Error = self::Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(StatusReportReasonCode::NoAdditionalInformation),
            1 => Ok(StatusReportReasonCode::LifetimeExpired),
            2 => Ok(StatusReportReasonCode::ForwardedOverUnidirectionalLink),
            3 => Ok(StatusReportReasonCode::TransmissionCanceled),
            4 => Ok(StatusReportReasonCode::DepletedStorage),
            5 => Ok(StatusReportReasonCode::DestinationEndpointIDUnavailable),
            6 => Ok(StatusReportReasonCode::NoKnownRouteToDestinationFromHere),
            7 => Ok(StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute),
            8 => Ok(StatusReportReasonCode::BlockUnintelligible),
            9 => Ok(StatusReportReasonCode::HopLimitExceeded),
            10 => Ok(StatusReportReasonCode::TrafficPared),
            11 => Ok(StatusReportReasonCode::BlockUnsupported),
            12 => Ok(StatusReportReasonCode::MissingSecurityOperation),
            13 => Ok(StatusReportReasonCode::UnknownSecurityOperation),
            14 => Ok(StatusReportReasonCode::UnexpectedSecurityOperation),
            15 => Ok(StatusReportReasonCode::FailedSecurityOperation),
            16 => Ok(StatusReportReasonCode::ConflictingSecurityOperation),
            255 => Err(Error::ReservedStatusReportReason),
            v => Ok(StatusReportReasonCode::Unassigned(v)),
        }
    }
}

#[derive(Debug)]
pub struct StatusAssertion(pub Option<time::OffsetDateTime>);

impl cbor::encode::ToCbor for StatusAssertion {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        if let Some(timestamp) = self.0 {
            encoder.emit_array(Some(2), |a| {
                a.emit(true);
                a.emit(bundle::to_dtn_time(&timestamp))
            })
        } else {
            encoder.emit_array(Some(1), |a| a.emit(true))
        }
    }
}

#[derive(Default, Debug)]
pub struct BundleStatusReport {
    pub bundle_id: bundle::BundleId,
    pub received: Option<StatusAssertion>,
    pub forwarded: Option<StatusAssertion>,
    pub delivered: Option<StatusAssertion>,
    pub deleted: Option<StatusAssertion>,
    pub reason: StatusReportReasonCode,
}

impl cbor::encode::ToCbor for BundleStatusReport {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit_array(Some(self.bundle_id.fragment_info.map_or(4, |_| 6)), |a| {
            // Statuses
            a.emit_array(Some(4), |a| {
                // This is a horrible format!
                if let Some(received) = self.received {
                    a.emit(received)
                } else {
                    a.emit(false)
                }
                if let Some(forwarded) = self.forwarded {
                    a.emit(forwarded)
                } else {
                    a.emit(false)
                }
                if let Some(delivered) = self.delivered {
                    a.emit(delivered)
                } else {
                    a.emit(false)
                }
                if let Some(deleted) = self.deleted {
                    a.emit(deleted)
                } else {
                    a.emit(false)
                }
            });

            // Reason code
            a.emit::<u64>(self.reason.into());
            // Source EID
            a.emit(&self.bundle_id.source);
            // Creation Timestamp
            a.emit(&self.bundle_id.timestamp);

            if let Some(fragment_info) = &self.bundle_id.fragment_info {
                // Add fragment info
                a.emit(fragment_info.offset);
                a.emit(fragment_info.total_len);
            }
        })
    }
}

impl cbor::decode::FromCbor for BundleStatusReport {
    type Error = self::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), Self::Error> {
        cbor::decode::parse_array(data, |a, tags| {
            let mut report = Self::default();
            a.parse_array(|a, _, tags| {
                if !tags.is_empty() {
                    trace!("Parsing administrative record with tags");
                }
                // This is a horrible format!
                if a.parse::<bool>().map_field_err("Received Status")? {
                    report.received = Some(StatusAssertion(
                        a.try_parse::<u64>()
                            .map(|milliseconds| milliseconds.map(bundle::from_dtn_time))
                            .map_field_err("Received Timestamp")?,
                    ))
                }
                if a.parse::<bool>().map_field_err("Forwarded Status")? {
                    report.forwarded = Some(StatusAssertion(
                        a.try_parse::<u64>()
                            .map(|milliseconds| milliseconds.map(bundle::from_dtn_time))
                            .map_field_err("Forwarded Timestamp")?,
                    ))
                }
                if a.parse::<bool>().map_field_err("Delivered Status")? {
                    report.delivered = Some(StatusAssertion(
                        a.try_parse::<u64>()
                            .map(|milliseconds| milliseconds.map(bundle::from_dtn_time))
                            .map_field_err("Delivered Timestamp")?,
                    ))
                }
                if a.parse::<bool>().map_field_err("Deleted Status")? {
                    report.deleted = Some(StatusAssertion(
                        a.try_parse::<u64>()
                            .map(|milliseconds| milliseconds.map(bundle::from_dtn_time))
                            .map_field_err("Deleted Timestamp")?,
                    ))
                }
                Ok::<(), Self::Error>(())
            })
            .map_field_err("Bundle Status Information")?;

            report.reason = a.parse::<u64>().map_field_err("Reason")?.try_into()?;

            report.bundle_id = bundle::BundleId {
                source: a.parse().map_field_err("Source")?,
                timestamp: a.parse().map_field_err("Timestamp")?,
                fragment_info: None,
            };

            if let Some(offset) = a.try_parse().map_field_err("Fragment offset")? {
                report.bundle_id.fragment_info = Some(FragmentInfo {
                    offset,
                    total_len: a.parse().map_field_err("Fragment length")?,
                });
            }
            Ok((report, tags.to_vec()))
        })
        .map(|((t, tags), len)| (t, len, tags))
    }
}

#[derive(Debug)]
pub enum AdministrativeRecord {
    BundleStatusReport(BundleStatusReport),
}

impl cbor::encode::ToCbor for AdministrativeRecord {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| match self {
            Self::BundleStatusReport(report) => {
                a.emit(1);
                a.emit(report);
            }
        })
    }
}

impl cbor::decode::FromCbor for AdministrativeRecord {
    type Error = self::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), Self::Error> {
        cbor::decode::parse_array(data, |a, tags| {
            match a.parse().map_field_err("Record Type Code")? {
                1u64 => {
                    let report = a.parse().map_field_err("Bundle Status Report")?;
                    if a.end()?.is_none() {
                        return Err(Error::AdditionalItems);
                    }
                    Ok((Self::BundleStatusReport(report), tags.to_vec()))
                }
                v => Err(Error::UnknownAdminRecordType(v)),
            }
        })
        .map(|((t, tags), len)| (t, len, tags))
    }
}
