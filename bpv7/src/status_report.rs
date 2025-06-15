use super::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Unknown administrative record type {0}")]
    UnknownAdminRecordType(u64),

    #[error("Reserved Status Report Reason Code (255)")]
    ReservedStatusReportReason,

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn core::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),
}

trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, Error>;
}

impl<T, E: Into<Box<dyn core::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for core::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, Error> {
        self.map_err(|e| Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasonCode {
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

impl From<ReasonCode> for u64 {
    fn from(value: ReasonCode) -> Self {
        match value {
            ReasonCode::NoAdditionalInformation => 0,
            ReasonCode::LifetimeExpired => 1,
            ReasonCode::ForwardedOverUnidirectionalLink => 2,
            ReasonCode::TransmissionCanceled => 3,
            ReasonCode::DepletedStorage => 4,
            ReasonCode::DestinationEndpointIDUnavailable => 5,
            ReasonCode::NoKnownRouteToDestinationFromHere => 6,
            ReasonCode::NoTimelyContactWithNextNodeOnRoute => 7,
            ReasonCode::BlockUnintelligible => 8,
            ReasonCode::HopLimitExceeded => 9,
            ReasonCode::TrafficPared => 10,
            ReasonCode::BlockUnsupported => 11,
            ReasonCode::MissingSecurityOperation => 12,
            ReasonCode::UnknownSecurityOperation => 13,
            ReasonCode::UnexpectedSecurityOperation => 14,
            ReasonCode::FailedSecurityOperation => 15,
            ReasonCode::ConflictingSecurityOperation => 16,
            ReasonCode::Unassigned(v) => v,
        }
    }
}

impl TryFrom<u64> for ReasonCode {
    type Error = Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ReasonCode::NoAdditionalInformation),
            1 => Ok(ReasonCode::LifetimeExpired),
            2 => Ok(ReasonCode::ForwardedOverUnidirectionalLink),
            3 => Ok(ReasonCode::TransmissionCanceled),
            4 => Ok(ReasonCode::DepletedStorage),
            5 => Ok(ReasonCode::DestinationEndpointIDUnavailable),
            6 => Ok(ReasonCode::NoKnownRouteToDestinationFromHere),
            7 => Ok(ReasonCode::NoTimelyContactWithNextNodeOnRoute),
            8 => Ok(ReasonCode::BlockUnintelligible),
            9 => Ok(ReasonCode::HopLimitExceeded),
            10 => Ok(ReasonCode::TrafficPared),
            11 => Ok(ReasonCode::BlockUnsupported),
            12 => Ok(ReasonCode::MissingSecurityOperation),
            13 => Ok(ReasonCode::UnknownSecurityOperation),
            14 => Ok(ReasonCode::UnexpectedSecurityOperation),
            15 => Ok(ReasonCode::FailedSecurityOperation),
            16 => Ok(ReasonCode::ConflictingSecurityOperation),
            255 => Err(Error::ReservedStatusReportReason),
            v => Ok(ReasonCode::Unassigned(v)),
        }
    }
}

impl hardy_cbor::encode::ToCbor for ReasonCode {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(u64::from(self))
    }
}

impl hardy_cbor::decode::FromCbor for ReasonCode {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        if let Some((v, shortest, len)) = hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data)?
        {
            Ok(Some((v.try_into()?, shortest, len)))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusAssertion(pub Option<dtn_time::DtnTime>);

fn emit_status_assertion(a: &mut hardy_cbor::encode::Array, sa: &Option<StatusAssertion>) {
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
    a: &mut hardy_cbor::decode::Array,
    shortest: &mut bool,
) -> Result<Option<StatusAssertion>, Error> {
    a.parse_array(|a, s, tags| {
        *shortest = *shortest && s && tags.is_empty() && a.is_definite();

        let status = a
            .parse()
            .map(|(v, s)| {
                *shortest = *shortest && s;
                v
            })
            .map_field_err("status")?;

        if status {
            if let Some(timestamp) = a
                .try_parse::<(dtn_time::DtnTime, bool)>()
                .map(|o| {
                    o.map(|(v, s)| {
                        *shortest = *shortest && s;
                        v
                    })
                })
                .map_field_err("timestamp")?
            {
                if timestamp.millisecs() == 0 {
                    Ok::<_, Error>(Some(StatusAssertion(None)))
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
}

#[derive(Default, Debug, Clone)]
pub struct BundleStatusReport {
    pub bundle_id: bundle::Id,
    pub received: Option<StatusAssertion>,
    pub forwarded: Option<StatusAssertion>,
    pub delivered: Option<StatusAssertion>,
    pub deleted: Option<StatusAssertion>,
    pub reason: ReasonCode,
}

impl hardy_cbor::encode::ToCbor for &BundleStatusReport {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
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

impl hardy_cbor::decode::FromCbor for BundleStatusReport {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse_array(data, |a, mut shortest, tags| {
            shortest = shortest && tags.is_empty() && a.is_definite();

            let mut report = Self::default();
            a.parse_array(|a, s, tags| {
                shortest = shortest && s && tags.is_empty() && a.is_definite();

                report.received =
                    parse_status_assertion(a, &mut shortest).map_field_err("received status")?;
                report.forwarded =
                    parse_status_assertion(a, &mut shortest).map_field_err("forwarded status")?;
                report.delivered =
                    parse_status_assertion(a, &mut shortest).map_field_err("delivered status")?;
                report.deleted =
                    parse_status_assertion(a, &mut shortest).map_field_err("deleted status")?;

                Ok::<_, Self::Error>(())
            })
            .map_field_err("bundle status information")?;

            report.reason = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("reason")?;

            let source = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("source")?;

            let timestamp = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("timestamp")?;

            report.bundle_id = bundle::Id {
                source,
                timestamp,
                fragment_info: None,
            };

            if let Some(offset) = a.try_parse().map_field_err("fragment offset")? {
                report.bundle_id.fragment_info = Some(bundle::FragmentInfo {
                    offset,
                    total_len: a.parse().map_field_err("fragment length")?,
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

impl hardy_cbor::encode::ToCbor for &AdministrativeRecord {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| match self {
            AdministrativeRecord::BundleStatusReport(report) => {
                a.emit(1);
                a.emit(report);
            }
        })
    }
}

impl hardy_cbor::decode::FromCbor for AdministrativeRecord {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse_array(data, |a, mut shortest, tags| {
            shortest = shortest && !tags.is_empty() && a.is_definite();

            match a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("record type code")?
            {
                1u64 => {
                    let (r, s) = a.parse().map_field_err("bundle status report")?;
                    Ok((Self::BundleStatusReport(r), shortest && s))
                }
                v => Err(Error::UnknownAdminRecordType(v)),
            }
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}
