// Errors for the server side of an exchange with a component.

use hardy_bpa::{cla, services};
use tonic::{Code, Status};

use crate::server::leases::Lease;

pub type Result<T> = core::result::Result<T, Error>;

// Why an exchange with the component behind a session failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // Lease expiry closes the whole session, not only this exchange.
    #[error("the client let the {0} lease expire")]
    LeaseExpired(Lease),

    #[error("the session closed before the exchange completed")]
    SessionClosed,

    #[error("cancelled by the client")]
    Cancelled,

    #[error("message not valid at this point in the transfer")]
    ProtocolViolation,

    #[error("the request stream closed before the transfer was acknowledged")]
    RequestStreamClosed,

    #[error("the request stream failed before the transfer was acknowledged")]
    RequestStreamFailed,

    #[error("the call ended during the transfer")]
    CallDropped,

    // The BPA-side segment source failed mid-transfer.
    #[error("the transfer ended before the final chunk")]
    TransferTruncated,

    #[error("the bundle is already being exchanged")]
    DuplicateAnnouncement,
}

impl Error {
    // The terminal status for the client's call, or `None` where the
    // call just ends.
    pub fn status(&self) -> Option<Status> {
        let code = match self {
            // The SDK maps UNAVAILABLE to `Disconnected`.
            Self::SessionClosed => Code::Unavailable,
            Self::LeaseExpired(_) => Code::DeadlineExceeded,
            Self::ProtocolViolation => Code::InvalidArgument,
            Self::RequestStreamClosed => Code::Cancelled,
            Self::RequestStreamFailed | Self::TransferTruncated => Code::Aborted,
            Self::Cancelled | Self::CallDropped | Self::DuplicateAnnouncement => return None,
        };
        Some(Status::new(code, self.to_string()))
    }

    // True when the component itself is gone rather than one exchange
    // failing. The match stays exhaustive so a new variant must pick a
    // side.
    fn is_disconnection(&self) -> bool {
        match self {
            Self::LeaseExpired(_) | Self::SessionClosed => true,
            Self::ProtocolViolation
            | Self::Cancelled
            | Self::RequestStreamClosed
            | Self::RequestStreamFailed
            | Self::CallDropped
            | Self::TransferTruncated
            | Self::DuplicateAnnouncement => false,
        }
    }
}

// `Disconnected` retires the registration; `StreamCancelled` fails
// this exchange alone.
impl From<Error> for services::Error {
    fn from(e: Error) -> Self {
        if e.is_disconnection() {
            Self::Disconnected
        } else {
            Self::StreamCancelled
        }
    }
}

// The same mapping for the CLA error type.
impl From<Error> for cla::Error {
    fn from(e: Error) -> Self {
        if e.is_disconnection() {
            Self::Disconnected
        } else {
            Self::StreamCancelled
        }
    }
}
