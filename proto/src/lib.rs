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
#[cfg(feature = "server")]
pub mod server;
pub mod stream;

/// Cap (in bytes) on a single encoded gRPC message in either direction.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// One slice of a data-plane transfer: large enough to amortise the
/// per-message overhead, small enough to interleave fairly with other
/// HTTP/2 streams on the connection.
pub const CHUNK_SIZE: usize = 256 * 1024;

/// The pre-flight bound on a transfer's declared size: a Send whose
/// metadata declares more than this is rejected before any bytes
/// arrive. Actual accumulation is bounded by the BPA's own maximum
/// bundle size as the stream is assembled, not by this constant —
/// bundle bytes are never materialised at the wire boundary.
pub const MAX_TRANSFER_SIZE: u64 = 8 * 1024 * 1024 * 1024;

/// The application surface: ADUs in and out.
pub mod application {
    tonic::include_proto!("application.v1");

    use hardy_bpa::{services, stream::Segment};

    use crate::stream;

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`stream`](crate::stream) grammar).
    impl stream::Chunk for SendRequest {
        fn chunk(segment: Segment) -> Self {
            Self {
                request: Some(match segment {
                    Segment::Next(bytes) => send_request::Request::Chunk(bytes),
                    Segment::Final(bytes) => send_request::Request::LastChunk(bytes),
                }),
            }
        }

        fn into_chunk(self) -> Option<Segment> {
            match self.request {
                Some(send_request::Request::Chunk(bytes)) => Some(Segment::Next(bytes)),
                Some(send_request::Request::LastChunk(bytes)) => Some(Segment::Final(bytes)),
                _ => None,
            }
        }
    }

    impl stream::Cancel for SendRequest {
        fn cancel() -> Self {
            Self {
                request: Some(send_request::Request::Cancel(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(self.request, Some(send_request::Request::Cancel(_)))
        }
    }

    impl stream::Chunk for ReceiveResponse {
        fn chunk(segment: Segment) -> Self {
            Self {
                response: Some(match segment {
                    Segment::Next(bytes) => receive_response::Response::Chunk(bytes),
                    Segment::Final(bytes) => receive_response::Response::LastChunk(bytes),
                }),
            }
        }

        fn into_chunk(self) -> Option<Segment> {
            match self.response {
                Some(receive_response::Response::Chunk(bytes)) => Some(Segment::Next(bytes)),
                Some(receive_response::Response::LastChunk(bytes)) => Some(Segment::Final(bytes)),
                _ => None,
            }
        }
    }

    // The withdrawal of a delivery mid-collection.
    impl stream::Cancel for ReceiveResponse {
        fn cancel() -> Self {
            Self {
                response: Some(receive_response::Response::Cancelled(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(
                self.response,
                Some(receive_response::Response::Cancelled(_))
            )
        }
    }

    // The abandonment of a collection.
    impl stream::Cancel for ReceiveRequest {
        fn cancel() -> Self {
            Self {
                request: Some(receive_request::Request::Cancel(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(self.request, Some(receive_request::Request::Cancel(_)))
        }
    }

    // The graceful end of a session.
    impl stream::Unregister for SubscribeRequest {
        fn is_unregister(&self) -> bool {
            matches!(
                self.request,
                Some(subscribe_request::Request::Unregister(_))
            )
        }
    }

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

    use hardy_bpa::{services, stream::Segment};

    use crate::stream;

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`stream`](crate::stream) grammar).
    impl stream::Chunk for SendRequest {
        fn chunk(segment: Segment) -> Self {
            Self {
                request: Some(match segment {
                    Segment::Next(bytes) => send_request::Request::Chunk(bytes),
                    Segment::Final(bytes) => send_request::Request::LastChunk(bytes),
                }),
            }
        }

        fn into_chunk(self) -> Option<Segment> {
            match self.request {
                Some(send_request::Request::Chunk(bytes)) => Some(Segment::Next(bytes)),
                Some(send_request::Request::LastChunk(bytes)) => Some(Segment::Final(bytes)),
                _ => None,
            }
        }
    }

    impl stream::Cancel for SendRequest {
        fn cancel() -> Self {
            Self {
                request: Some(send_request::Request::Cancel(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(self.request, Some(send_request::Request::Cancel(_)))
        }
    }

    impl stream::Chunk for ReceiveResponse {
        fn chunk(segment: Segment) -> Self {
            Self {
                response: Some(match segment {
                    Segment::Next(bytes) => receive_response::Response::Chunk(bytes),
                    Segment::Final(bytes) => receive_response::Response::LastChunk(bytes),
                }),
            }
        }

        fn into_chunk(self) -> Option<Segment> {
            match self.response {
                Some(receive_response::Response::Chunk(bytes)) => Some(Segment::Next(bytes)),
                Some(receive_response::Response::LastChunk(bytes)) => Some(Segment::Final(bytes)),
                _ => None,
            }
        }
    }

    // The withdrawal of a delivery mid-collection.
    impl stream::Cancel for ReceiveResponse {
        fn cancel() -> Self {
            Self {
                response: Some(receive_response::Response::Cancelled(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(
                self.response,
                Some(receive_response::Response::Cancelled(_))
            )
        }
    }

    // The abandonment of a collection.
    impl stream::Cancel for ReceiveRequest {
        fn cancel() -> Self {
            Self {
                request: Some(receive_request::Request::Cancel(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(self.request, Some(receive_request::Request::Cancel(_)))
        }
    }

    // The graceful end of a session.
    impl stream::Unregister for SubscribeRequest {
        fn is_unregister(&self) -> bool {
            matches!(
                self.request,
                Some(subscribe_request::Request::Unregister(_))
            )
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

/// The convergence-layer adapter surface.
pub mod cla {
    tonic::include_proto!("cla.v1");

    use hardy_bpa::{cla, stream::Segment};
    use tonic::Status;

    use crate::stream;

    // The chunked-transfer capabilities of this surface's data-plane
    // messages (the [`stream`](crate::stream) grammar).
    impl stream::Chunk for DispatchRequest {
        fn chunk(segment: Segment) -> Self {
            Self {
                request: Some(match segment {
                    Segment::Next(bytes) => dispatch_request::Request::Chunk(bytes),
                    Segment::Final(bytes) => dispatch_request::Request::LastChunk(bytes),
                }),
            }
        }

        fn into_chunk(self) -> Option<Segment> {
            match self.request {
                Some(dispatch_request::Request::Chunk(bytes)) => Some(Segment::Next(bytes)),
                Some(dispatch_request::Request::LastChunk(bytes)) => Some(Segment::Final(bytes)),
                _ => None,
            }
        }
    }

    impl stream::Cancel for DispatchRequest {
        fn cancel() -> Self {
            Self {
                request: Some(dispatch_request::Request::Cancel(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(self.request, Some(dispatch_request::Request::Cancel(_)))
        }
    }

    impl stream::Chunk for ForwardResponse {
        fn chunk(segment: Segment) -> Self {
            Self {
                response: Some(match segment {
                    Segment::Next(bytes) => forward_response::Response::Chunk(bytes),
                    Segment::Final(bytes) => forward_response::Response::LastChunk(bytes),
                }),
            }
        }

        fn into_chunk(self) -> Option<Segment> {
            match self.response {
                Some(forward_response::Response::Chunk(bytes)) => Some(Segment::Next(bytes)),
                Some(forward_response::Response::LastChunk(bytes)) => Some(Segment::Final(bytes)),
                _ => None,
            }
        }
    }

    // The withdrawal of a forwarding mid-transfer.
    impl stream::Cancel for ForwardResponse {
        fn cancel() -> Self {
            Self {
                response: Some(forward_response::Response::Cancelled(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(
                self.response,
                Some(forward_response::Response::Cancelled(_))
            )
        }
    }

    // The abandonment of a forwarding.
    impl stream::Cancel for ForwardRequest {
        fn cancel() -> Self {
            Self {
                request: Some(forward_request::Request::Cancel(())),
            }
        }

        fn is_cancel(&self) -> bool {
            matches!(self.request, Some(forward_request::Request::Cancel(_)))
        }
    }

    // The graceful end of a session.
    impl stream::Unregister for SubscribeRequest {
        fn is_unregister(&self) -> bool {
            matches!(
                self.request,
                Some(subscribe_request::Request::Unregister(_))
            )
        }
    }

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

    impl TryFrom<ClaAddress> for cla::ClaAddress {
        type Error = Status;

        fn try_from(address: ClaAddress) -> Result<Self, Status> {
            let Some(address_type) = Option::<cla::ClaAddressType>::from(address.address_type())
            else {
                return Err(Status::invalid_argument("Unspecified address type"));
            };
            Self::try_from((address_type, address.address))
                .map_err(|e| Status::invalid_argument(format!("Invalid address: {e}")))
        }
    }
}

/// The routing agent surface.
pub mod routing {
    tonic::include_proto!("routing.v1");

    use tonic::Status;

    use crate::stream;

    // The graceful end of a session.
    impl stream::Unregister for SubscribeRequest {
        fn is_unregister(&self) -> bool {
            matches!(
                self.request,
                Some(subscribe_request::Request::Unregister(_))
            )
        }
    }

    impl From<&hardy_bpa::routing::RouteAction> for RouteAction {
        fn from(action: &hardy_bpa::routing::RouteAction) -> Self {
            use hardy_bpa::routing::RouteAction as Domain;
            let action = match action {
                Domain::Drop(reason) => route_action::Action::Drop(Drop {
                    reason_code: reason.map(u64::from),
                }),
                Domain::Reflect => route_action::Action::Reflect(()),
                Domain::Via(eid) => route_action::Action::Via(eid.to_string()),
            };
            Self {
                action: Some(action),
            }
        }
    }

    // The wire route action's oneof becomes the domain's, resolving the
    // via EID and the drop reason code; a bad via EID errors with a gRPC
    // `Status`.
    impl TryFrom<route_action::Action> for hardy_bpa::routing::RouteAction {
        type Error = Status;

        fn try_from(action: route_action::Action) -> Result<Self, Status> {
            use hardy_bpv7::status_report::ReasonCode;
            Ok(match action {
                route_action::Action::Drop(drop) => Self::Drop(
                    drop.reason_code
                        .map(|c| ReasonCode::try_from(c).unwrap_or(ReasonCode::Unassigned(c))),
                ),
                route_action::Action::Reflect(_) => Self::Reflect,
                route_action::Action::Via(eid) => Self::Via(
                    eid.parse::<hardy_bpv7::eid::Eid>()
                        .map_err(|e| Status::invalid_argument(format!("Invalid via EID: {e}")))?,
                ),
            })
        }
    }
}
