// The client SDK surfaces, one file per wire surface, mirroring the
// server's `services/` layout. Each opens a Subscribe session, hands the
// component a sink whose calls are the wire's token-gated RPCs, and (for
// the surfaces that receive them) translates events onto the local
// trait. Shared conversions live here; the wire-to-segment adapters
// (both directions) are `super::adapter`.

pub mod application;
pub mod cla;
pub mod routing;
pub mod service;

use hardy_bpa::services;
use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use prost_types::Timestamp;
use time::OffsetDateTime;
use tonic::{Code, Status};

// Wire statuses become service errors, uniformly across the surfaces: a
// dead token or an unreachable BPA is the sink's disconnection,
// everything else carries through.
fn service_error(status: Status) -> services::Error {
    match status.code() {
        Code::Unauthenticated | Code::Unavailable => services::Error::Disconnected,
        _ => services::Error::Internal(status.into()),
    }
}

fn from_timestamp(t: Timestamp) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(t.seconds)
        .ok()
        .map(|seconds| seconds + time::Duration::nanoseconds(t.nanos.into()))
}

// Decodes the fields every surface's status report shares; the
// per-surface assertion enum converts at the call site. `None` is a
// malformed report, logged and skipped by the session loop.
fn decode_status_report(
    bundle_id: &str,
    reporting_node: &str,
    reason_code: u64,
    status_time: Option<Timestamp>,
) -> Option<(Id, Eid, ReasonCode, Option<OffsetDateTime>)> {
    let bundle_id = Id::from_key(bundle_id).ok()?;
    let from = reporting_node.parse().ok()?;
    let reason = ReasonCode::try_from(reason_code).unwrap_or(ReasonCode::Unassigned(reason_code));
    Some((
        bundle_id,
        from,
        reason,
        status_time.and_then(from_timestamp),
    ))
}
