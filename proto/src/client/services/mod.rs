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

use core::ops::ControlFlow;

use hardy_async::CancellationToken;
use hardy_bpa::services;
use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use prost_types::Timestamp;
use time::{Duration, OffsetDateTime};
use tonic::{Code, Status, Streaming};
use tracing::{debug, warn};

use crate::error_status::recover_service_error;

// Wire statuses become service errors, uniformly across the surfaces. A
// status carrying the wire's typed-error discriminator recovers as the
// exact domain error the server raised; otherwise (a non-Hardy server,
// or a kind whose payload cannot travel) the status code classifies it:
// a dead token or an unreachable BPA is the sink's disconnection, a
// cancelled call is the stream's cancellation (the same error a local
// streamed send returns when its producer gives up before the final
// segment), everything else carries through.
fn service_error(status: Status) -> services::Error {
    if let Some(e) = recover_service_error(&status) {
        return e;
    }
    match status.code() {
        Code::Unauthenticated | Code::Unavailable => services::Error::Disconnected,
        Code::Cancelled => services::Error::StreamCancelled,
        _ => services::Error::Internal(status.into()),
    }
}

// A session-ending status, bubbled to the registration handle with
// nothing flattened: a status carrying the wire's typed-error
// discriminator recovers as the exact domain error the server raised;
// any other ending (the transport failing under the stream, a foreign
// server) is carried whole, its source chain intact, so awaiting the
// handle shows the actual failure. The doors classify instead
// (`service_error`): their callers react to the category, where a
// session's ending is read by a human or an exit path.
fn session_error(status: Status) -> services::Error {
    recover_service_error(&status).unwrap_or_else(|| services::Error::Internal(status.into()))
}

// Advances a session's event stream: `Continue(event)` yields the next
// event to handle, and `Break` ends the session — `Break(None)` a clean
// end (the client's pool cancelled, or the server half-closed the
// stream: a round-tripped unregister or the BPA's own shutdown,
// indistinguishable on the wire), `Break(Some(status))` a stream failure
// carrying its gRPC status. A surface bubbles that status through its
// session-error conversion, so the registration handle hands the caller
// the real error rather than a flattened one.
async fn next_event<M>(
    events: &mut Streaming<M>,
    cancel: &CancellationToken,
) -> ControlFlow<Option<Status>, M> {
    let message = tokio::select! {
        biased;
        _ = cancel.cancelled() => return ControlFlow::Break(None),
        message = events.message() => message,
    };
    match message {
        Ok(Some(message)) => ControlFlow::Continue(message),
        Ok(None) => ControlFlow::Break(None),
        Err(status) => {
            warn!("Subscribe stream failed: {status}");
            ControlFlow::Break(Some(status))
        }
    }
}

// Reports a delivery the component declined (its `on_deliver` returned
// `Err`). The commit point is the client's ack, so a decline never acks:
// the server parks the bundle for a later registration either way. A
// decline after full receipt is still notable (the component took the
// whole ADU and refused it) so it warns; an incomplete one is the routine
// deferral. `surface` names the trait for the log ("Application"/
// "Service"); the caller abandons the collection by dropping it.
fn log_declined(surface: &str, id: &str, stream_completed: bool, e: &services::Error) {
    if stream_completed {
        warn!(
            "{surface} declined delivery {id} after receiving it in full; it will be re-delivered: {e}"
        );
    } else {
        debug!("{surface} declined delivery {id}: {e}");
    }
}

fn from_timestamp(t: Timestamp) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(t.seconds)
        .ok()
        .map(|seconds| seconds + Duration::nanoseconds(t.nanos.into()))
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
    // An unknown code decodes as `Unassigned`; only the reserved 255 is
    // an error here, and a report carrying it is malformed like any
    // other undecodable field.
    let reason = ReasonCode::try_from(reason_code).ok()?;
    Some((
        bundle_id,
        from,
        reason,
        status_time.and_then(from_timestamp),
    ))
}
