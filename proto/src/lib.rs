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
in [`CHUNK_SIZE`] slices.

Without features, this crate is the contract alone: the generated
wire types and their domain conversions. The [`client`] module
(behind the `client` feature) is the SDK: it lets a local component
register against a remote BPA with the same traits a local `Bpa`
uses. The [`server`] module (behind the `server` feature) is the
other end: the bridges a host mounts to serve these surfaces against
its own `hardy_bpa::Bpa`.
*/

#[cfg(feature = "client")]
pub mod client;
pub mod error_status;
#[cfg(feature = "server")]
pub mod server;
pub mod stream;

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

// The stream grammar (see [`stream`]) is spoken by ten generated message
// types across the four surfaces, every impl identical modulo the message
// type, its oneof field, and the oneof's path. These macros keep each
// surface module a declaration list instead of ninety lines of repeated
// match arms; the variant names (`Chunk`/`LastChunk`, the cancel variant,
// `Unregister`) are fixed by the schemas.

// [`stream::Chunk`]: the data-carrying half of a transfer.
macro_rules! impl_chunk {
    ($msg:ty, $field:ident, $oneof:ty) => {
        impl $crate::stream::Chunk for $msg {
            fn chunk(segment: hardy_bpa::stream::Segment) -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(match segment {
                        hardy_bpa::stream::Segment::Next(bytes) => Oneof::Chunk(bytes),
                        hardy_bpa::stream::Segment::Final(bytes) => Oneof::LastChunk(bytes),
                    }),
                }
            }

            fn into_chunk(self) -> Option<hardy_bpa::stream::Segment> {
                type Oneof = $oneof;
                match self.$field {
                    Some(Oneof::Chunk(bytes)) => Some(hardy_bpa::stream::Segment::Next(bytes)),
                    Some(Oneof::LastChunk(bytes)) => Some(hardy_bpa::stream::Segment::Final(bytes)),
                    _ => None,
                }
            }
        }
    };
}

// [`stream::Cancel`]: the in-band abort, whose variant name each schema
// picks to read naturally in its direction (`Cancel` on requests,
// `Cancelled` on responses).
macro_rules! impl_cancel {
    ($msg:ty, $field:ident, $oneof:ty, $cancel:ident) => {
        impl $crate::stream::Cancel for $msg {
            fn cancel() -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(Oneof::$cancel(())),
                }
            }

            fn is_cancel(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::$cancel(_)))
            }
        }
    };
}

// [`stream::Ack`]: the in-band acknowledgement that commits a collection.
macro_rules! impl_ack {
    ($msg:ty, $field:ident, $oneof:ty, $ack:ident) => {
        impl $crate::stream::Ack for $msg {
            fn ack() -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(Oneof::$ack(())),
                }
            }

            fn is_ack(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::$ack(_)))
            }
        }
    };
}

// [`stream::Unregister`]: the graceful end of a session.
macro_rules! impl_unregister {
    ($msg:ty, $field:ident, $oneof:ty) => {
        impl $crate::stream::Unregister for $msg {
            fn is_unregister(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::Unregister(_)))
            }
        }
    };
}

/// The application surface: ADUs in and out.
pub mod application {
    tonic::include_proto!("application.v1");

    use hardy_bpa::services;

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`stream`](crate::stream) grammar).
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

    // The graceful end of a session.
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
    tonic::include_proto!("service.v1");

    use hardy_bpa::services;

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`stream`](crate::stream) grammar).
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

    // The graceful end of a session.
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

/// The convergence-layer adapter surface.
pub mod cla {
    tonic::include_proto!("cla.v1");

    use hardy_bpa::cla;
    use tonic::Status;

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`stream`](crate::stream) grammar).
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

    // The graceful end of a session.
    impl_unregister!(SubscribeRequest, request, subscribe_request::Request);

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
    tonic::include_proto!("routing.v1");

    // Aliased to keep the domain type distinct from the wire `RouteAction`
    // this module generates.
    use hardy_bpa::routing::RouteAction as DomainRouteAction;
    use hardy_bpv7::{
        eid::{self, Eid},
        status_report::ReasonCode,
    };
    use tonic::Status;

    // The graceful end of a session.
    impl_unregister!(SubscribeRequest, request, subscribe_request::Request);

    /// A wire route action that names no domain action.
    #[derive(Debug, thiserror::Error)]
    pub enum RouteActionError {
        /// The `via` endpoint id does not parse.
        #[error("Invalid via EID: {0}")]
        InvalidVia(#[from] eid::Error),
    }

    // The doors answer a bad route action as a plain invalid-argument.
    impl From<RouteActionError> for Status {
        fn from(e: RouteActionError) -> Self {
            Status::invalid_argument(e.to_string())
        }
    }

    impl From<&DomainRouteAction> for RouteAction {
        fn from(action: &DomainRouteAction) -> Self {
            let action = match action {
                DomainRouteAction::Drop(reason) => route_action::Action::Drop(Drop {
                    reason_code: reason.map(u64::from),
                }),
                DomainRouteAction::Reflect => route_action::Action::Reflect(()),
                DomainRouteAction::Via(eid) => route_action::Action::Via(eid.to_string()),
            };
            Self {
                action: Some(action),
            }
        }
    }

    // The wire route action's oneof becomes the domain's, resolving the
    // via EID and the drop reason code.
    impl TryFrom<route_action::Action> for DomainRouteAction {
        type Error = RouteActionError;

        fn try_from(action: route_action::Action) -> Result<Self, RouteActionError> {
            Ok(match action {
                route_action::Action::Drop(drop) => Self::Drop(
                    drop.reason_code
                        .map(|c| ReasonCode::try_from(c).unwrap_or(ReasonCode::Unassigned(c))),
                ),
                route_action::Action::Reflect(_) => Self::Reflect,
                route_action::Action::Via(eid) => Self::Via(eid.parse::<Eid>()?),
            })
        }
    }
}
