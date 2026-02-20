/*!
This module provides functionality for creating, parsing, and managing BPv7 bundle status reports.

A `BundleStatusReport` is a type of `AdministrativeRecord` used in a Bundle Protocol agent to report
on the status of a bundle. This can include events like bundle reception, forwarding, delivery, and deletion.
*/

use super::*;
use crate::error::CaptureFieldErr;
use thiserror::Error;

/// Errors that can occur when working with status reports.
#[derive(Error, Debug)]
pub enum Error {
    /// Indicates that an unknown administrative record type was encountered.
    #[error("Unknown administrative record type {0}")]
    UnknownAdminRecordType(u64),

    /// Indicates that a reserved and unassigned reason code (255) was used.
    #[error("Reserved Status Report Reason Code (255)")]
    ReservedStatusReportReason,

    /// Error resulting from a failure to parse a field within the status report.
    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn core::error::Error + Send + Sync>,
    },

    /// Error resulting from invalid CBOR data.
    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),
}

impl crate::error::HasInvalidField for Error {
    fn invalid_field(
        field: &'static str,
        source: Box<dyn core::error::Error + Send + Sync>,
    ) -> Self {
        Error::InvalidField { field, source }
    }
}

/// Represents the reason for a bundle status report.
///
/// These codes are defined in the BPv7 specification and indicate why the status report was generated.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasonCode {
    /// No additional information is available.
    #[default]
    NoAdditionalInformation,
    /// The bundle's lifetime has expired.
    LifetimeExpired,
    /// The bundle was forwarded over a unidirectional link.
    ForwardedOverUnidirectionalLink,
    /// The transmission of the bundle was canceled.
    TransmissionCanceled,
    /// The bundle was deleted due to depleted storage.
    DepletedStorage,
    /// The destination endpoint ID was unavailable.
    DestinationEndpointIDUnavailable,
    /// There is no known route to the destination from the reporting node.
    NoKnownRouteToDestinationFromHere,
    /// There was no timely contact with the next node on the route.
    NoTimelyContactWithNextNodeOnRoute,
    /// A block in the bundle was unintelligible.
    BlockUnintelligible,
    /// The bundle's hop limit was exceeded.
    HopLimitExceeded,
    /// Traffic was pared (i.e., some bundles were dropped).
    TrafficPared,
    /// A block in the bundle is unsupported.
    BlockUnsupported,
    /// A required security operation was missing.
    MissingSecurityOperation,
    /// An unknown security operation was encountered.
    UnknownSecurityOperation,
    /// An unexpected security operation was encountered.
    UnexpectedSecurityOperation,
    /// A security operation failed.
    FailedSecurityOperation,
    /// A conflicting security operation was encountered.
    ConflictingSecurityOperation,
    /// An unassigned reason code.
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
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&u64::from(*self))
    }
}

impl hardy_cbor::decode::FromCbor for ReasonCode {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (v, shortest, len) = hardy_cbor::decode::parse::<(u64, bool, usize)>(data)?;
        Ok((v.try_into()?, shortest, len))
    }
}

/// Represents a status assertion, which may include a timestamp.
///
/// A `StatusAssertion` is used to indicate that a particular event (e.g., reception, forwarding)
/// has occurred. It can optionally include the time at which the event happened.
#[derive(Debug, Clone)]
pub struct StatusAssertion(pub Option<time::OffsetDateTime>);

fn emit_status_assertion(a: &mut hardy_cbor::encode::Array, sa: &Option<StatusAssertion>) {
    // This is a horrible format!
    match sa {
        None => a.emit(&[false]),
        Some(StatusAssertion(None)) => a.emit(&[true]),
        Some(StatusAssertion(Some(timestamp))) => {
            a.emit(&(true, dtn_time::DtnTime::saturating_from(*timestamp)))
        }
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
            .map_field_err::<Error>("status")?;

        if status {
            if let Some(timestamp) = a
                .try_parse::<(dtn_time::DtnTime, bool)>()
                .map(|o| {
                    o.map(|(v, s)| {
                        *shortest = *shortest && s;
                        v
                    })
                })
                .map_field_err::<Error>("timestamp")?
            {
                if timestamp.millisecs() == 0 {
                    Ok::<_, Error>(Some(StatusAssertion(None)))
                } else {
                    Ok(Some(StatusAssertion(Some(timestamp.into()))))
                }
            } else {
                Ok(Some(StatusAssertion(None)))
            }
        } else {
            Ok(None)
        }
    })
}

/// Represents a bundle status report.
///
/// This struct contains information about the status of a bundle, including which events
/// have occurred (reception, forwarding, delivery, deletion) and the reason for the report.
#[derive(Default, Debug, Clone)]
pub struct BundleStatusReport {
    /// The ID of the bundle that this report pertains to.
    pub bundle_id: bundle::Id,
    /// Status assertion for when the bundle was received.
    pub received: Option<StatusAssertion>,
    /// Status assertion for when the bundle was forwarded.
    pub forwarded: Option<StatusAssertion>,
    /// Status assertion for when the bundle was delivered.
    pub delivered: Option<StatusAssertion>,
    /// Status assertion for when the bundle was deleted.
    pub deleted: Option<StatusAssertion>,
    /// The reason for this status report.
    pub reason: ReasonCode,
}

impl hardy_cbor::encode::ToCbor for BundleStatusReport {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit_array(
            Some(if self.bundle_id.fragment_info.is_none() {
                4
            } else {
                6
            }),
            |a| {
                // Statuses
                a.emit_array(Some(4), |a| {
                    emit_status_assertion(a, &self.received);
                    emit_status_assertion(a, &self.forwarded);
                    emit_status_assertion(a, &self.delivered);
                    emit_status_assertion(a, &self.deleted);
                });

                // Reason code
                a.emit(&self.reason);
                // Source EID
                a.emit(&self.bundle_id.source);
                // Creation Timestamp
                a.emit(&self.bundle_id.timestamp);

                if let Some(fragment_info) = &self.bundle_id.fragment_info {
                    // Add fragment info
                    a.emit(&fragment_info.offset);
                    a.emit(&fragment_info.total_adu_length);
                }
            },
        )
    }
}

impl hardy_cbor::decode::FromCbor for BundleStatusReport {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, mut shortest, tags| {
            shortest = shortest && tags.is_empty() && a.is_definite();

            let mut report = Self::default();
            a.parse_array(|a, s, tags| {
                shortest = shortest && s && tags.is_empty() && a.is_definite();

                report.received = parse_status_assertion(a, &mut shortest)
                    .map_field_err::<Error>("received status")?;
                report.forwarded = parse_status_assertion(a, &mut shortest)
                    .map_field_err::<Error>("forwarded status")?;
                report.delivered = parse_status_assertion(a, &mut shortest)
                    .map_field_err::<Error>("delivered status")?;
                report.deleted = parse_status_assertion(a, &mut shortest)
                    .map_field_err::<Error>("deleted status")?;

                Ok::<_, Self::Error>(())
            })
            .map_field_err::<Error>("bundle status information")?;

            report.reason = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err::<Error>("reason")?;

            let source = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err::<Error>("source")?;

            let timestamp = a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err::<Error>("timestamp")?;

            report.bundle_id = bundle::Id {
                source,
                timestamp,
                fragment_info: None,
            };

            if let Some(offset) = a.try_parse().map_field_err::<Error>("fragment offset")? {
                report.bundle_id.fragment_info = Some(bundle::FragmentInfo {
                    offset,
                    total_adu_length: a
                        .parse()
                        .map_field_err::<Error>("fragment total ADU length")?,
                });
            }
            Ok((report, shortest))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

/// Represents an administrative record.
///
/// An administrative record is a special type of bundle payload that is used for network
/// management purposes. The only type currently supported is the `BundleStatusReport`.
#[derive(Debug)]
pub enum AdministrativeRecord {
    /// A bundle status report.
    BundleStatusReport(BundleStatusReport),
}

impl hardy_cbor::encode::ToCbor for AdministrativeRecord {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        match self {
            AdministrativeRecord::BundleStatusReport(report) => encoder.emit(&(1, report)),
        }
    }
}

impl hardy_cbor::decode::FromCbor for AdministrativeRecord {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, mut shortest, tags| {
            shortest = shortest && !tags.is_empty() && a.is_definite();

            match a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err::<Error>("record type code")?
            {
                1u64 => {
                    let (r, s) = a.parse().map_field_err::<Error>("bundle status report")?;
                    Ok((Self::BundleStatusReport(r), shortest && s))
                }
                v => Err(Error::UnknownAdminRecordType(v)),
            }
        })
        .map(|((v, s), len)| (v, s, len))
    }
}
