use anyhow::anyhow;
use hardy_cbor as cbor;
use std::collections::HashMap;

mod crc;

pub mod bundle_builder;
pub mod parse;

pub use bundle_builder::*;
pub use hardy_bpa_core::bundle::*;
pub use parse::parse_bundle;

pub fn dtn_time(instant: &time::OffsetDateTime) -> u64 {
    (*instant - time::macros::datetime!(2000-01-01 00:00:00 UTC)).whole_milliseconds() as u64
}

pub enum StatusReportReasonCode {
    NoAdditionalInformation = 0,
    LifetimeExpired = 1,
    ForwardedOverUnidirectionalLink = 2,
    TransmissionCanceled = 3,
    DepletedStorage = 4,
    DestinationEndpointIDUnavailable = 5,
    NoKnownRouteToDestinationFromHere = 6,
    NoTimelyContactWithNextNodeOnRoute = 7,
    BlockUnintelligible = 8,
    HopLimitExceeded = 9,
    TrafficPared = 10,
    BlockUnsupported = 11,
    MissingSecurityOperation = 12,
    UnknownSecurityOperation = 13,
    UnexpectedSecurityOperation = 14,
    FailedSecurityOperation = 15,
    ConflictingSecurityOperation = 16,
}
