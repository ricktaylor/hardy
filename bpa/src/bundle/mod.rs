use super::*;
use hardy_cbor as cbor;

mod builder;
mod crc;
mod dtn_time;
mod editor;
mod node_id;
mod parse;

pub use builder::*;
pub use dtn_time::*;
pub use editor::*;
pub use hardy_bpa_core::bundle::*;
pub use node_id::*;
pub use parse::parse;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
