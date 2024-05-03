use anyhow::anyhow;
use hardy_cbor as cbor;
use std::collections::HashMap;

mod crc;

pub mod builder;
pub mod dtn_time;
pub mod editor;
pub mod parse;

pub use builder::*;
pub use dtn_time::{as_dtn_time, has_bundle_expired};
pub use editor::*;
pub use hardy_bpa_core::bundle::*;
pub use parse::parse;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

impl From<StatusReportReasonCode> for u64 {
    fn from(value: StatusReportReasonCode) -> Self {
        value as u64
    }
}

impl cbor::encode::ToCbor for StatusReportReasonCode {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit::<u64>(self.into())
    }
}
