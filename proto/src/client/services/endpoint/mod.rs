//! The two endpoint APIs, `hardy.application.v1` and `hardy.service.v1`, and
//! what they share.
//!
//! Both register an endpoint on the BPA and exchange bundles with it, one as
//! ADUs and one whole, so both read a server's refusal the same way.

pub mod application;
pub mod service;

use hardy_bpa::services;
use hardy_bpv7::eid::Service;
use tonic::{Code, Status};

/// Maps the status a session stream ends with to the component's error.
///
/// The status is a gRPC code and a message, so this reads the code: a code the
/// server only ever sends when it has ended the session is `Disconnected`, and
/// anything else, which includes a transport failure, is `Internal`, which
/// keeps the status and so its message.
fn session_error(status: Status) -> services::Error {
    match status.code() {
        Code::Unauthenticated | Code::Unavailable | Code::DeadlineExceeded => {
            services::Error::Disconnected
        }
        _ => services::Error::Internal(status.into()),
    }
}

/// Maps a status from a data-plane call to the component's error.
///
/// As [`session_error`], and in addition a code the server sends for a single
/// exchange ends that exchange: a cancelled or truncated transfer is
/// `StreamCancelled`, a bundle the BPA already holds is `DuplicateBundle`, and
/// a bundle a filter refused is `Dropped`.
fn transfer_error(status: Status) -> services::Error {
    match status.code() {
        Code::Cancelled | Code::Aborted => services::Error::StreamCancelled,
        Code::AlreadyExists => services::Error::DuplicateBundle,
        Code::FailedPrecondition => services::Error::Dropped(None),
        // Every deadline the server sends is a stall it has already closed the
        // session over, so it is a disconnection and not one lost exchange.
        _ => session_error(status),
    }
}

/// Maps a status from the registration handshake to the component's error.
///
/// As [`session_error`], except that `ALREADY_EXISTS` is `service_id` held by
/// another session and `FAILED_PRECONDITION` is the node lacking a node id of
/// the scheme `service_id` asked for. The caller passes the id because the
/// status does not carry it: the client asked for it, so it already knows
/// which one was refused, and a request for a service number of the BPA's
/// choosing asks for the ipn scheme. A BPA choosing the number has no id to
/// hold against the request, so `ALREADY_EXISTS` on such a request is not a
/// contract answer and stays `Internal`.
fn registration_error(status: Status, service_id: Option<&Service>) -> services::Error {
    match (status.code(), service_id) {
        (Code::AlreadyExists, Some(Service::Ipn(n))) => {
            services::Error::ServiceIdInUse(n.to_string())
        }
        (Code::AlreadyExists, Some(Service::Dtn(name))) => {
            services::Error::ServiceIdInUse(name.to_string())
        }
        (Code::AlreadyExists, None) => services::Error::Internal(status.into()),
        (Code::FailedPrecondition, Some(Service::Dtn(_))) => services::Error::NoDtnNodeId,
        (Code::FailedPrecondition, Some(Service::Ipn(_)) | None) => services::Error::NoIpnNodeId,
        _ => session_error(status),
    }
}
