// The client SDK surfaces, one file per wire surface, mirroring the
// server's layout. Each opens a Subscribe session, hands the component
// a sink whose calls are the wire's token-gated RPCs, and translates
// events onto the local trait. The segment adapters are
// `super::adapter`.

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

// A status carrying the wire's typed-error discriminator recovers as
// the exact domain error the server raised; otherwise the status code
// classifies it.
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
// nothing flattened: any unclassified ending is carried whole, source
// chain intact, so awaiting the handle shows the actual failure. The
// doors classify instead, because their callers react to the category.
fn session_error(status: Status) -> services::Error {
    recover_service_error(&status).unwrap_or_else(|| services::Error::Internal(status.into()))
}

// Whether this side asked the session to end. A solicited close and the
// BPA dropping the registration both arrive as a half-closed stream, so
// the SDK records its own half as it happens. The sink records, the
// event loop reads once the stream ends.
#[derive(Default)]
struct LocalUnregister(AtomicBool);

impl LocalUnregister {
    // The sink sent `Unregister`, or was dropped, which half-closes the
    // request stream and the BPA treats the same way.
    fn record(&self) {
        self.0.store(true, Ordering::Release);
    }

    // Whether a session that just ended ended at this side's asking: an
    // unregister of ours, or the client's shutdown.
    fn solicited(&self, cancel: &CancellationToken) -> bool {
        self.0.load(Ordering::Acquire) || cancel.is_cancelled()
    }
}

// Advances a session's event stream. `Break(None)` is an end without a
// failure, which the caller classifies against [`LocalUnregister`];
// `Break(Some(status))` is a stream failure, which the surface bubbles
// through its session-error conversion so the registration handle hands
// the caller the real error.
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
