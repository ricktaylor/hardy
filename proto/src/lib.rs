/*!
The gRPC wire contract of the Hardy BPA: one service per component
surface, generated from the schemas in `proto/`. The surfaces are
applications ([`application`]), low-level services ([`service`]),
convergence-layer adapters ([`cla`]), and routing agents ([`routing`]).

On each surface, `Subscribe` registers the component and then streams
events from the BPA; every other RPC is gated by the session token
minted at registration. Payload bytes move only on the streaming
data-plane RPCs, in [`CHUNK_SIZE`] slices, using the chunked-transfer
grammar in [`grammar`].

# Feature Flags

- `client`: the [`client`] SDK, which registers a local component
  against a remote BPA through the same traits a local `Bpa` takes.
- `server`: the [`server`] services, which a host mounts to serve
  these surfaces from its own `hardy_bpa::Bpa`.
- `instrument`: `tracing` spans on the client SDK and server services.

With no features enabled the crate contains only the wire contract:
the generated types, their domain conversions, [`grammar`], and
[`status`].
*/
// The in-crate server tests build a full `Bpa` inside one async fn;
// the resulting future exceeds the default recursion limit.
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

/// Maximum size in bytes of a single encoded gRPC message, in either
/// direction.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Size in bytes of one slice of a data-plane transfer.
///
/// Kept well under [`MAX_MESSAGE_SIZE`] so a chunk message always
/// fits, and small enough that concurrent streams interleave.
pub const CHUNK_SIZE: usize = 1024 * 1024;

/// Default HTTP/2 DATA frame size: [`CHUNK_SIZE`], clamped to the
/// HTTP/2 maximum frame size (RFC 9113 section 6.5.2).
pub const DEFAULT_MAX_FRAME_SIZE: u32 = {
    let max = (1u32 << 24) - 1;
    if CHUNK_SIZE < max as usize {
        CHUNK_SIZE as u32
    } else {
        max
    }
};

/// Upper bound in bytes on a transfer's declared size.
///
/// A transfer declaring more than this is rejected before any payload
/// bytes are read.
pub const MAX_TRANSFER_SIZE: u64 = 8 * 1024 * 1024 * 1024;

// Copy of the BPA's private lane-count clamp `MAX_EAGER_LANE_QUEUES`,
// which is not exported; the two can drift (see `docs/TODO.md`).
#[cfg(any(feature = "client", feature = "server"))]
pub(crate) const MAX_LANE_COUNT: u32 = 256;

/// The application surface: sending and receiving ADUs.
pub mod application {
    tonic::include_proto!("hardy.application.v1");

    use hardy_bpa::services;

    use crate::grammar::{impl_ack, impl_cancel, impl_chunk, impl_unregister};

    impl_chunk!(SendRequest, request, send_request::Request);

    impl_cancel!(SendRequest, request, send_request::Request, Cancel);

    impl_chunk!(ReceiveResponse, response, receive_response::Response);

    // `Cancelled` on the response: the BPA withdraws a delivery
    // mid-collection.
    impl_cancel!(
        ReceiveResponse,
        response,
        receive_response::Response,
        Cancelled
    );

    // `Cancel` on the request: the application abandons the collection.
    impl_cancel!(ReceiveRequest, request, receive_request::Request, Cancel);

    impl_ack!(ReceiveRequest, request, receive_request::Request, Ack);

    impl_unregister!(SubscribeRequest, request, subscribe_request::Request);

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

/// The low-level service surface: sending and receiving whole BPv7
/// bundles.
pub mod service {
    tonic::include_proto!("hardy.service.v1");

    use hardy_bpa::services;

    use crate::grammar::{impl_ack, impl_cancel, impl_chunk, impl_unregister};

    impl_chunk!(SendRequest, request, send_request::Request);

    impl_cancel!(SendRequest, request, send_request::Request, Cancel);

    impl_chunk!(ReceiveResponse, response, receive_response::Response);

    // `Cancelled` on the response: the BPA withdraws a delivery
    // mid-collection.
    impl_cancel!(
        ReceiveResponse,
        response,
        receive_response::Response,
        Cancelled
    );

    // `Cancel` on the request: the service abandons the collection.
    impl_cancel!(ReceiveRequest, request, receive_request::Request, Cancel);

    impl_ack!(ReceiveRequest, request, receive_request::Request, Ack);

    impl_unregister!(SubscribeRequest, request, subscribe_request::Request);

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

/// The convergence-layer adapter surface: dispatching and forwarding
/// bundles.
pub mod cla {
    tonic::include_proto!("hardy.cla.v1");

    use hardy_bpa::cla;
    use tonic::Status;

    use crate::grammar::{impl_cancel, impl_chunk, impl_unregister};

    impl_chunk!(DispatchRequest, request, dispatch_request::Request);

    impl_cancel!(DispatchRequest, request, dispatch_request::Request, Cancel);

    impl_chunk!(ForwardResponse, response, forward_response::Response);

    // `Cancelled` on the response: the BPA withdraws a forwarding
    // mid-transfer.
    impl_cancel!(
        ForwardResponse,
        response,
        forward_response::Response,
        Cancelled
    );

    // `Cancel` on the request: the CLA abandons the forwarding.
    impl_cancel!(ForwardRequest, request, forward_request::Request, Cancel);

    impl_unregister!(SubscribeRequest, request, subscribe_request::Request);

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

    /// An error converting a wire [`ClaAddress`] into a domain address.
    #[derive(Debug, thiserror::Error)]
    pub enum AddressError {
        /// The wire message left the address type unspecified.
        #[error("Unspecified address type")]
        Unspecified,

        /// The address bytes do not parse as the named type.
        #[error("Invalid address: {0}")]
        Invalid(#[from] cla::Error),
    }

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

/// The routing agent surface: adding and removing routes.
pub mod routing {
    tonic::include_proto!("hardy.routing.v1");

    use crate::grammar::impl_unregister;

    impl_unregister!(SubscribeRequest, request, subscribe_request::Request);

    // Aliased: the generated wire type is also named `RouteAction`.
    use hardy_bpa::routing::RouteAction as DomainRouteAction;
    use hardy_bpv7::{
        eid::{self, Eid},
        status_report::ReasonCode,
    };
    use tonic::Status;

    /// An error converting between wire and domain route actions.
    #[derive(Debug, thiserror::Error)]
    pub enum RouteActionError {
        /// The `via` endpoint id does not parse.
        #[error("Invalid via EID: {0}")]
        InvalidVia(#[from] eid::Error),
        /// The drop reason code is the reserved value 255.
        #[error("Reserved status report reason code")]
        ReservedReason,
    }

    impl From<RouteActionError> for Status {
        fn from(e: RouteActionError) -> Self {
            Status::invalid_argument(e.to_string())
        }
    }

    // A drop reason of `Unassigned(255)` carries the reserved code, so
    // it is refused rather than encoded.
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

    impl TryFrom<route_action::Action> for DomainRouteAction {
        type Error = RouteActionError;

        fn try_from(action: route_action::Action) -> Result<Self, RouteActionError> {
            Ok(match action {
                // `ReasonCode::try_from` rejects only the reserved 255.
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
