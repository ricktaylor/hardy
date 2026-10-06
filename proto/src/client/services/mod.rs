//! One session type per API, and the helpers they share.
//!
//! Each API's `*Session` is created by its `subscribe`, which performs the
//! registration handshake and hands the component its sink, and then driven by
//! `handle_events`, which runs the event loop to the session's end.

pub mod cla;
pub mod endpoint;
pub mod routing;

use core::ops::ControlFlow;

use hardy_async::CancellationToken;
use tonic::{Status, Streaming};
use tracing::debug;

use crate::grammar::Registration;

/// The depth of the request channel feeding a data-plane call: one message may
/// queue while tonic sends the one before it.
pub const TRANSFER_REQUEST_CAPACITY: usize = 2;

/// The depth of the request channel feeding a session's `Subscribe` call.
pub const SUBSCRIBE_REQUEST_CAPACITY: usize = 4;

/// Reads the `Registration` event that must open a session's response stream.
///
/// # Errors
///
/// Returns the stream's own status if it fails, and `INTERNAL` if the stream
/// ends or yields any other event first. No Hardy server decided that
/// `INTERNAL`: it is the client's own reading of an answer the contract does
/// not allow.
async fn expect_registration<M: Registration>(
    events: &mut Streaming<M>,
) -> Result<M::Registration, Status> {
    events
        .message()
        .await?
        .and_then(Registration::into_registration)
        .ok_or_else(|| Status::internal("the session stream did not open with a Registration"))
}

/// Reads the next event from a session stream, unless `cancel` fires first.
///
/// `Break(None)` means the session ended cleanly, by cancellation or by the
/// stream closing; `Break(Some(status))` means it failed.
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
            debug!("session stream failed: {status}");
            ControlFlow::Break(Some(status))
        }
    }
}
