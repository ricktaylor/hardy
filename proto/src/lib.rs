/*!
The gRPC wire contract of the Hardy BPA.

One service per component surface, generated from the schemas in
`proto/`: applications ([`application`]), low-level services
([`service`]), convergence-layer adapters ([`cla`]), and routing
agents ([`routing`]).

Each surface follows the same design: `Subscribe` is the session (a
registration handshake, then a pure event stream from the BPA), and
every other interaction is an RPC gated by the session token minted at
registration. Payload bytes move only on the streaming data-plane RPCs,
in [`CHUNK_SIZE`] slices, speaking the chunked-transfer grammar of
[`grammar`]; the [`transfer`] module sequences a whole transfer of
them.

Without features, this crate is the contract alone: the generated
wire types and their domain conversions. The [`client`] module
(behind the `client` feature) is the SDK: it lets a local component
register against a remote BPA with the same traits a local `Bpa`
uses. The [`server`] module (behind the `server` feature) is the
other end: the rpc services a host mounts to serve these surfaces
against its own `hardy_bpa::Bpa`.
*/
// The in-crate server tests build a whole `Bpa` inside one async fn; the
// layout query for that state machine exceeds the default depth limit.
#![cfg_attr(test, recursion_limit = "256")]

#[cfg(feature = "client")]
pub mod client;
pub mod grammar;
#[cfg(feature = "server")]
pub mod server;
pub mod status;
#[cfg(any(feature = "client", feature = "server"))]
mod timestamp;
#[cfg(any(feature = "client", feature = "server"))]
mod token;
#[cfg(any(feature = "client", feature = "server"))]
mod transfer;

/// Cap (in bytes) on a single encoded gRPC message in either direction.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// One slice of a data-plane transfer: large enough to amortise the
/// per-message overhead (encode, framing, a channel hop) across a
/// GB-scale transfer, small enough to stay well under [`MAX_MESSAGE_SIZE`]
/// and to interleave fairly with other HTTP/2 streams (whose pacing the
/// adaptive flow-control window governs, not this size).
pub const CHUNK_SIZE: usize = 1024 * 1024;

/// The default HTTP/2 DATA frame cap for the SDK client and, unless the
/// operator overrides it, the server: sized to carry a whole
/// [`CHUNK_SIZE`] slice in one frame, but clamped to HTTP/2's maximum
/// frame size (`2^24 - 1`, RFC 9113 §6.5.2) so a chunk raised toward
/// [`MAX_MESSAGE_SIZE`] simply spans more frames rather than producing an
/// out-of-range setting.
pub const DEFAULT_MAX_FRAME_SIZE: u32 = {
    let max = (1u32 << 24) - 1;
    if CHUNK_SIZE < max as usize {
        CHUNK_SIZE as u32
    } else {
        max
    }
};

/// The pre-flight bound on a transfer's declared size: a Send whose
/// metadata declares more than this is rejected before any bytes
/// arrive. Actual accumulation is bounded by the BPA's own maximum
/// bundle size as the stream is assembled, not by this constant —
/// bundle bytes are never materialised at the wire boundary.
pub const MAX_TRANSFER_SIZE: u64 = 8 * 1024 * 1024 * 1024;

// The bound the CLA registration doors enforce on a declared lane
// count, mirroring the limit the BPA clamps to. A copy because the
// BPA's own bound is private today; it goes away with the public one
// (see [`docs/TODO.md`](../docs/TODO.md)).
#[cfg(any(feature = "client", feature = "server"))]
pub(crate) const MAX_LANE_COUNT: u32 = 256;

/// The application surface: ADUs in and out.
pub mod application {
    tonic::include_proto!("hardy.application.v1");

    use hardy_bpa::services;

    use crate::grammar::{impl_ack, impl_cancel, impl_chunk};

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`grammar`](crate::grammar)).
    impl_chunk!(SendRequest, request, send_request::Request);

    impl_cancel!(SendRequest, request, send_request::Request, Cancel);

    impl_chunk!(ReceiveResponse, response, receive_response::Response);

    // The withdrawal of a delivery mid-collection.
    impl_cancel!(
        ReceiveResponse,
        response,
        receive_response::Response,
        Cancelled
    );

    // The abandonment of a collection.
    impl_cancel!(ReceiveRequest, request, receive_request::Request, Cancel);

    // The acknowledgement that commits a collection.
    impl_ack!(ReceiveRequest, request, receive_request::Request, Ack);

    impl From<SendOptions> for services::SendOptions {
        fn from(o: SendOptions) -> Self {
            Self {
                do_not_fragment: o.do_not_fragment,
                request_ack: o.app_ack_requested,
                report_status_time: o.report_status_time,
                notify_reception: o.report_reception,
                notify_forwarding: o.report_forwarding,
                notify_delivery: o.report_delivery,
                notify_deletion: o.report_deletion,
            }
        }
    }

    impl From<services::SendOptions> for SendOptions {
        fn from(o: services::SendOptions) -> Self {
            Self {
                do_not_fragment: o.do_not_fragment,
                app_ack_requested: o.request_ack,
                report_status_time: o.report_status_time,
                report_reception: o.notify_reception,
                report_forwarding: o.notify_forwarding,
                report_delivery: o.notify_delivery,
                report_deletion: o.notify_deletion,
            }
        }
    }

    impl From<services::StatusNotify> for StatusAssertion {
        fn from(kind: services::StatusNotify) -> Self {
            match kind {
                services::StatusNotify::Received => Self::Received,
                services::StatusNotify::Forwarded => Self::Forwarded,
                services::StatusNotify::Delivered => Self::Delivered,
                services::StatusNotify::Deleted => Self::Deleted,
            }
        }
    }

    // `Unspecified` has no domain meaning, so the wire value maps to
    // `None` (the consumer skips the report) rather than erroring.
    impl From<StatusAssertion> for Option<services::StatusNotify> {
        fn from(assertion: StatusAssertion) -> Self {
            match assertion {
                StatusAssertion::Received => Some(services::StatusNotify::Received),
                StatusAssertion::Forwarded => Some(services::StatusNotify::Forwarded),
                StatusAssertion::Delivered => Some(services::StatusNotify::Delivered),
                StatusAssertion::Deleted => Some(services::StatusNotify::Deleted),
                StatusAssertion::Unspecified => None,
            }
        }
    }
}

/// The low-level service surface: whole BPv7 bundles in and out.
pub mod service {
    tonic::include_proto!("hardy.service.v1");

    use hardy_bpa::services;

    use crate::grammar::{impl_ack, impl_cancel, impl_chunk};

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`grammar`](crate::grammar)).
    impl_chunk!(SendRequest, request, send_request::Request);

    impl_cancel!(SendRequest, request, send_request::Request, Cancel);

    impl_chunk!(ReceiveResponse, response, receive_response::Response);

    // The withdrawal of a delivery mid-collection.
    impl_cancel!(
        ReceiveResponse,
        response,
        receive_response::Response,
        Cancelled
    );

    // The abandonment of a collection.
    impl_cancel!(ReceiveRequest, request, receive_request::Request, Cancel);

    // The acknowledgement that commits a collection.
    impl_ack!(ReceiveRequest, request, receive_request::Request, Ack);

    impl From<services::StatusNotify> for StatusAssertion {
        fn from(kind: services::StatusNotify) -> Self {
            match kind {
                services::StatusNotify::Received => Self::Received,
                services::StatusNotify::Forwarded => Self::Forwarded,
                services::StatusNotify::Delivered => Self::Delivered,
                services::StatusNotify::Deleted => Self::Deleted,
            }
        }
    }

    impl From<StatusAssertion> for Option<services::StatusNotify> {
        fn from(assertion: StatusAssertion) -> Self {
            match assertion {
                StatusAssertion::Received => Some(services::StatusNotify::Received),
                StatusAssertion::Forwarded => Some(services::StatusNotify::Forwarded),
                StatusAssertion::Delivered => Some(services::StatusNotify::Delivered),
                StatusAssertion::Deleted => Some(services::StatusNotify::Deleted),
                StatusAssertion::Unspecified => None,
            }
        }
    }
}

/// The convergence-layer adapter surface.
pub mod cla {
    tonic::include_proto!("hardy.cla.v1");

    use hardy_bpa::cla;
    use tonic::Status;

    use crate::grammar::{impl_cancel, impl_chunk};

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`grammar`](crate::grammar)).
    impl_chunk!(DispatchRequest, request, dispatch_request::Request);

    impl_cancel!(DispatchRequest, request, dispatch_request::Request, Cancel);

    impl_chunk!(ForwardResponse, response, forward_response::Response);

    // The withdrawal of a forwarding mid-transfer.
    impl_cancel!(
        ForwardResponse,
        response,
        forward_response::Response,
        Cancelled
    );

    // The abandonment of a forwarding.
    impl_cancel!(ForwardRequest, request, forward_request::Request, Cancel);

    // `Unspecified` names no domain type, so it maps to `None`.
    impl From<ClaAddressType> for Option<cla::ClaAddressType> {
        fn from(address_type: ClaAddressType) -> Self {
            match address_type {
                ClaAddressType::Unspecified => None,
                ClaAddressType::Tcp => Some(cla::ClaAddressType::Tcp),
                ClaAddressType::Private => Some(cla::ClaAddressType::Private),
            }
        }
    }

    impl From<cla::ClaAddressType> for ClaAddressType {
        fn from(address_type: cla::ClaAddressType) -> Self {
            match address_type {
                cla::ClaAddressType::Tcp => Self::Tcp,
                cla::ClaAddressType::Private => Self::Private,
            }
        }
    }

    impl From<cla::ClaAddress> for ClaAddress {
        fn from(address: cla::ClaAddress) -> Self {
            let (address_type, address) = address.into();
            Self {
                address_type: ClaAddressType::from(address_type) as i32,
                address,
            }
        }
    }

    /// A wire [`ClaAddress`] that names no domain address.
    #[derive(Debug, thiserror::Error)]
    pub enum AddressError {
        /// The wire message left the address type unspecified.
        #[error("Unspecified address type")]
        Unspecified,

        /// The address bytes do not parse as the named type.
        #[error("Invalid address: {0}")]
        Invalid(#[from] cla::Error),
    }

    // The doors answer a bad address as a plain invalid-argument.
    impl From<AddressError> for Status {
        fn from(e: AddressError) -> Self {
            Status::invalid_argument(e.to_string())
        }
    }

    impl TryFrom<ClaAddress> for cla::ClaAddress {
        type Error = AddressError;

        fn try_from(address: ClaAddress) -> Result<Self, AddressError> {
            let Some(address_type) = Option::<cla::ClaAddressType>::from(address.address_type())
            else {
                return Err(AddressError::Unspecified);
            };
            Ok(Self::try_from((address_type, address.address))?)
        }
    }
}

/// The routing agent surface.
pub mod routing {
    tonic::include_proto!("hardy.routing.v1");

    // Aliased to keep the domain type distinct from the wire `RouteAction`
    // this module generates.
    use hardy_bpa::routing::RouteAction as DomainRouteAction;
    use hardy_bpv7::{
        eid::{self, Eid},
        status_report::ReasonCode,
    };
    use tonic::Status;

    /// A wire route action that names no domain action.
    #[derive(Debug, thiserror::Error)]
    pub enum RouteActionError {
        /// The `via` endpoint id does not parse.
        #[error("Invalid via EID: {0}")]
        InvalidVia(#[from] eid::Error),
        /// The drop reason code is reserved and cannot be carried.
        #[error("Reserved status report reason code")]
        ReservedReason,
    }

    // The doors answer a bad route action as a plain invalid-argument.
    impl From<RouteActionError> for Status {
        fn from(e: RouteActionError) -> Self {
            Status::invalid_argument(e.to_string())
        }
    }

    // A drop reason the receiving end would reject as reserved is
    // refused before it is sent, so neither direction carries one.
    impl TryFrom<&DomainRouteAction> for RouteAction {
        type Error = RouteActionError;

        fn try_from(action: &DomainRouteAction) -> Result<Self, RouteActionError> {
            let action = match action {
                DomainRouteAction::Drop(reason) => {
                    let reason_code = reason.map(u64::from);
                    if reason_code.is_some_and(|code| ReasonCode::try_from(code).is_err()) {
                        return Err(RouteActionError::ReservedReason);
                    }
                    route_action::Action::Drop(Drop { reason_code })
                }
                DomainRouteAction::Reflect => route_action::Action::Reflect(()),
                DomainRouteAction::Via(eid) => route_action::Action::Via(eid.to_string()),
            };
            Ok(Self {
                action: Some(action),
            })
        }
    }

    // The wire route action's oneof becomes the domain's, resolving the
    // via EID and the drop reason code.
    impl TryFrom<route_action::Action> for DomainRouteAction {
        type Error = RouteActionError;

        fn try_from(action: route_action::Action) -> Result<Self, RouteActionError> {
            Ok(match action {
                // An unknown code becomes `Unassigned`; the reserved
                // 255 is refused rather than laundered.
                route_action::Action::Drop(drop) => Self::Drop(
                    drop.reason_code
                        .map(ReasonCode::try_from)
                        .transpose()
                        .map_err(|_| RouteActionError::ReservedReason)?,
                ),
                route_action::Action::Reflect(_) => Self::Reflect,
                route_action::Action::Via(eid) => Self::Via(eid.parse::<Eid>()?),
            })
        }
    }
}
