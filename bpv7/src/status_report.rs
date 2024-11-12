use super::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StatusReportError {
    #[error("Unknown administrative record type {0}")]
    UnknownAdminRecordType(u64),

    #[error("Reserved Status Report Reason Code (255)")]
    ReservedStatusReportReason,

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),
}

trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, StatusReportError>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, StatusReportError> {
        self.map_err(|e| StatusReportError::InvalidField {
            field,
            source: e.into(),
        })
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusReportReasonCode {
    #[default]
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
    type Error = self::StatusReportError;

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
            255 => Err(StatusReportError::ReservedStatusReportReason),
            v => Ok(StatusReportReasonCode::Unassigned(v)),
        }
    }
}

impl cbor::encode::ToCbor for StatusReportReasonCode {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(u64::from(self))
    }
}

impl cbor::decode::FromCbor for StatusReportReasonCode {
    type Error = StatusReportError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = cbor::decode::try_parse::<(u64, bool, usize)>(data)? {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusAssertion(pub Option<DtnTime>);

fn emit_status_assertion(a: &mut cbor::encode::Array, sa: &Option<StatusAssertion>) {
    // This is a horrible format!
    match sa {
        None => a.emit_array(Some(1), |a| {
            a.emit(false);
        }),
        Some(StatusAssertion(None)) => a.emit_array(Some(1), |a| {
            a.emit(true);
        }),
        Some(StatusAssertion(Some(timestamp))) => a.emit_array(Some(2), |a| {
            a.emit(true);
            a.emit(*timestamp);
        }),
    }
}

fn parse_status_assertion(
    a: &mut cbor::decode::Array,
    shortest: &mut bool,
) -> Result<Option<StatusAssertion>, StatusReportError> {
    a.parse_array(|a, s, tags| {
        *shortest = *shortest && s && tags.is_empty() && a.is_definite();

        let status = a
            .parse()
            .map(|(v, s)| {
                *shortest = *shortest && s;
                v
            })
            .map_field_err("Status")?;

        if status {
            if let Some(timestamp) = a
                .try_parse::<(DtnTime, bool)>()
                .map(|o| {
                    o.map(|(v, s)| {
                        *shortest = *shortest && s;
                        v
                    })
                })
                .map_field_err("Timestamp")?
            {
                if timestamp.millisecs() == 0 {
                    Ok::<_, StatusReportError>(Some(StatusAssertion(None)))
                } else {
                    Ok(Some(StatusAssertion(Some(timestamp))))
                }
            } else {
                Ok(Some(StatusAssertion(None)))
            }
        } else {
            Ok(None)
        }
    })
    .map(|o| o.0)
}

#[derive(Default, Debug, Clone)]
pub struct BundleStatusReport {
    pub bundle_id: BundleId,
    pub received: Option<StatusAssertion>,
    pub forwarded: Option<StatusAssertion>,
    pub delivered: Option<StatusAssertion>,
    pub deleted: Option<StatusAssertion>,
    pub reason: StatusReportReasonCode,
}

impl cbor::encode::ToCbor for &BundleStatusReport {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit_array(
            Some(self.bundle_id.fragment_info.as_ref().map_or(4, |_| 6)),
            |a| {
                // Statuses
                a.emit_array(Some(4), |a| {
                    emit_status_assertion(a, &self.received);
                    emit_status_assertion(a, &self.forwarded);
                    emit_status_assertion(a, &self.delivered);
                    emit_status_assertion(a, &self.deleted);
                });

                // Reason code
                a.emit(self.reason);
                // Source EID
                a.emit(&self.bundle_id.source);
                // Creation Timestamp
                a.emit(&self.bundle_id.timestamp);

                if let Some(fragment_info) = &self.bundle_id.fragment_info {
                    // Add fragment info
                    a.emit(fragment_info.offset);
                    a.emit(fragment_info.total_len);
                }
            },
        )
    }
}

impl cbor::decode::FromCbor for BundleStatusReport {
    type Error = StatusReportError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |a, mut shortest, tags| {
            shortest = shortest && tags.is_empty() && a.is_definite();

            let mut report = Self::default();
            a.parse_array(|a, s, tags| {
                shortest = shortest && s && tags.is_empty() && a.is_definite();

                report.received =
                    parse_status_assertion(a, &mut shortest).map_field_err("Received Status")?;
                report.forwarded =
                    parse_status_assertion(a, &mut shortest).map_field_err("Forwarded Status")?;
                report.delivered =
                    parse_status_assertion(a, &mut shortest).map_field_err("Delivered Status")?;
                report.deleted =
                    parse_status_assertion(a, &mut shortest).map_field_err("Deleted Status")?;

                Ok::<_, Self::Error>(())
            })
            .map_field_err("Bundle Status Information")?;

            report.reason = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("Reason")?;

            let source = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("Source")?;

            let timestamp = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("Timestamp")?;

            report.bundle_id = BundleId {
                source,
                timestamp,
                fragment_info: None,
            };

            if let Some(offset) = a.try_parse().map_field_err("Fragment offset")? {
                report.bundle_id.fragment_info = Some(FragmentInfo {
                    offset,
                    total_len: a.parse().map_field_err("Fragment length")?,
                });
            }
            Ok((report, shortest))
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}

#[derive(Debug)]
pub enum AdministrativeRecord {
    BundleStatusReport(BundleStatusReport),
}

impl cbor::encode::ToCbor for &AdministrativeRecord {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| match self {
            AdministrativeRecord::BundleStatusReport(report) => {
                a.emit(1);
                a.emit(report);
            }
        })
    }
}

impl cbor::decode::FromCbor for AdministrativeRecord {
    type Error = self::StatusReportError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |a, mut shortest, tags| {
            shortest = shortest && !tags.is_empty() && a.is_definite();

            match a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("Record Type Code")?
            {
                1u64 => {
                    let (r, s) = a.parse().map_field_err("Bundle Status Report")?;
                    Ok((Self::BundleStatusReport(r), shortest && s))
                }
                v => Err(StatusReportError::UnknownAdminRecordType(v)),
            }
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}
