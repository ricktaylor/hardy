// Per-surface client sessions: application, service, CLA, and
// routing, plus helpers they share.

pub mod application;
pub mod cla;
pub mod routing;
pub mod service;

use core::{
    ops::ControlFlow,
    sync::atomic::{AtomicBool, Ordering},
};

use hardy_async::CancellationToken;
use hardy_bpa::services;
use tonic::{Code, Status, Streaming};
use tracing::warn;

use crate::status::recover_service_error;

// Maps a wire status to a domain error: the embedded discriminator if
// present, otherwise a classification by status code.
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

// Maps a session's final status: the embedded discriminator if
// present, otherwise `Internal` carrying the status as its source.
fn session_error(status: Status) -> services::Error {
    recover_service_error(&status).unwrap_or_else(|| services::Error::Internal(status.into()))
}

// Records that this side asked the session to end. The flag is needed
// because a solicited close and a BPA-side drop both arrive as the
// same half-closed stream.
#[derive(Default)]
struct LocalUnregister(AtomicBool);

impl LocalUnregister {
    // Called when the sink sends `Unregister` or is dropped.
    fn record(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn solicited(&self, cancel: &CancellationToken) -> bool {
        self.0.load(Ordering::Acquire) || cancel.is_cancelled()
    }
}

// Advances a session's event stream. `Break(None)` is a clean end or
// a cancellation; `Break(Some(status))` is a stream failure.
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
