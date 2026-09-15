//! The statuses a Hardy server ends a call with when the BPA refuses it.
//!
//! `proto` is a boundary, so it holds no error vocabulary of its own. Outward
//! it speaks [`Status`]; inward it speaks the BPA's own error types. A call
//! handler writes the status for a fault it finds itself, where it finds it,
//! and tells the BPA its own verdict where a collection ends; this module holds
//! the one translation that must be written once, a BPA error becoming the
//! status its call ends with.
//!
//! A status is a gRPC code and a message, and nothing else. The code is the
//! machine-readable part, and is all a proxy, a retry policy or a dashboard
//! reads. The message is a short phrase for a developer reading a log; it rides
//! the `grpc-message` header percent-encoded on every failed call, so it stays
//! short, and nothing a client sent is reflected back in it: a client already
//! holds its own request, and a message whose length is the client's to choose
//! can push the status past the HTTP/2 header cap and arrive as nothing at
//! all.

use core::fmt::Display;

use hardy_bpa::{cla, routing, services};
use hardy_bpv7::status_report::ReasonCode;
use tonic::Status;
use tracing::{debug, error};

use crate::cla::AddressError;

/// The server failed for a cause it does not disclose. `cause` is logged at
/// `error` and never sent.
pub fn internal(cause: impl Display) -> Status {
    error!("internal error: {cause}");
    Status::internal("internal error")
}

/// Turns a BPA service failure into the status the call ends with.
///
/// The mapping is many-to-one and deliberately lossy: nothing in the status
/// names a BPA type. What the SDK cannot rebuild from the status and the
/// request it already holds becomes the component's own `Internal`, which keeps
/// the status whole.
pub fn service_status(error: services::Error) -> Status {
    match error {
        services::Error::ServiceIdInUse(id) => {
            debug!("service id {id} is registered on another session");
            Status::already_exists("service id already registered")
        }
        services::Error::DtnInvalidServiceName(_) => {
            Status::invalid_argument("Register.dtn is not a valid dtn service name")
        }
        services::Error::AdministrativeEndpoint(_) => Status::invalid_argument(
            "Register.service_id names the administrative endpoint, which cannot be registered",
        ),
        services::Error::NoIpnNodeId | services::Error::NoDtnNodeId => {
            debug!("no node id this registration can use: {error}");
            Status::failed_precondition("the node has no node id of the requested scheme")
        }
        services::Error::NodeId(ref inner) => {
            debug!("no node id this registration can use: {inner}");
            Status::failed_precondition("the node's node id cannot carry this service")
        }
        services::Error::Disconnected => Status::unavailable("registration closed"),
        services::Error::PayloadTooLarge { size, max } => Status::resource_exhausted(format!(
            "the ADU of {size} bytes exceeds the limit of {max} bytes"
        )),
        services::Error::PayloadUnderrun { size, expected } => Status::invalid_argument(format!(
            "the ADU was declared as {expected} bytes, only {size} arrived"
        )),
        services::Error::PayloadUnaddressable { total_len } => Status::resource_exhausted(format!(
            "the ADU of {total_len} bytes cannot be addressed on this node"
        )),
        services::Error::InvalidDestination(_) => {
            Status::invalid_argument("the bundle's source is not the registered endpoint")
        }
        services::Error::StreamCancelled => Status::cancelled("transfer cancelled"),
        // The reason code is named only if it is one RFC 9171 lets a report
        // carry: the round trip through `u64` drops the reserved code 255,
        // which `ReasonCode` can hold as unassigned but no report may state.
        services::Error::Dropped(reason) => Status::failed_precondition(
            match reason
                .map(u64::from)
                .filter(|code| ReasonCode::try_from(*code).is_ok())
            {
                Some(code) => format!("bundle refused by a filter, reason code {code}"),
                None => "bundle refused by a filter".to_string(),
            },
        ),
        services::Error::DuplicateBundle => Status::already_exists("duplicate bundle"),
        services::Error::InvalidBundle(_) => {
            Status::invalid_argument("the bytes are not a valid bundle")
        }
        services::Error::Internal(ref inner) => internal(inner),
    }
}

/// As [`service_status`], for the CLA API.
pub fn cla_status(error: cla::Error) -> Status {
    match error {
        cla::Error::AlreadyExists(name) => {
            debug!("cla {name} is registered on another session");
            Status::already_exists("cla already registered")
        }
        cla::Error::Disconnected => Status::unavailable("registration closed"),
        cla::Error::StreamCancelled => Status::cancelled("transfer cancelled"),
        cla::Error::PayloadTooLarge { size, max } => Status::resource_exhausted(format!(
            "the bundle of {size} bytes exceeds the limit of {max} bytes"
        )),
        cla::Error::PayloadUnderrun { size, expected } => Status::invalid_argument(format!(
            "the bundle was declared as {expected} bytes, only {size} arrived"
        )),
        cla::Error::PayloadUnaddressable { total_len } => Status::resource_exhausted(format!(
            "the bundle of {total_len} bytes cannot be addressed on this node"
        )),
        cla::Error::Internal(ref inner) => internal(inner),
    }
}

/// Names the field of a wire `ClaAddress` at fault.
impl From<AddressError> for Status {
    fn from(error: AddressError) -> Self {
        Status::invalid_argument(match error {
            AddressError::Unspecified => "ClaAddress.type is unspecified or unknown",
            AddressError::Invalid(_) => "ClaAddress.address is not an address of its type",
        })
    }
}

/// As [`service_status`], for the routing API.
pub fn routing_status(error: routing::agent::Error) -> Status {
    match error {
        routing::agent::Error::AlreadyExists(name) => {
            debug!("routing agent {name} is registered on another session");
            Status::already_exists("routing agent already registered")
        }
        routing::agent::Error::Disconnected => Status::unavailable("registration closed"),
        routing::agent::Error::NullNextHop => {
            Status::invalid_argument("RouteAction.via is the null endpoint")
        }
        routing::agent::Error::ViaOwnNode(_) => {
            Status::invalid_argument("RouteAction.via is this node")
        }
        routing::agent::Error::Internal(ref inner) => internal(inner),
    }
}
