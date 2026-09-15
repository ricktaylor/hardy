/*!
The gRPC wire contract of the Hardy Bundle Protocol Agent (BPA), with an
optional server and client SDK.

A BPA implementing [RFC 9171][rfc9171] Bundle Protocol Version 7 works through
attached components: applications that exchange application data units (ADUs),
services that exchange complete bundles, convergence-layer adapters (CLAs) that
connect it to links, and routing agents that supply routes. This crate lets a
component run in another process or on another machine. It compiles the four
protobuf schemas under `proto/` into one Rust module each. Behind feature flags,
it also implements both ends: the server a BPA mounts, and the client a
component registers through. Neither side speaks gRPC by hand.

# APIs

Each schema defines one API in its own versioned protobuf package. Each is
generated into one root module, with its tonic client and server.

| Module | Package | Component |
| --- | --- | --- |
| [`application`] | `hardy.application.v1` | An application exchanging ADUs. |
| [`service`] | `hardy.service.v1` | A service exchanging complete bundles. |
| [`cla`] | `hardy.cla.v1` | A CLA passing bundles to and from a link. |
| [`routing`] | `hardy.routing.v1` | A routing agent supplying routes. |

# How a component talks to the BPA

A component registers with the BPA and stays registered while it works. During
that time the BPA pushes events to it. On the wire, the registration is a
*session*. Every API builds it from the same three pieces.

1. **The session is one `Subscribe` call.** The first request is `Register`. The
   first response is a `Registration` event carrying a session token. After
   that, the response stream carries only events. Bundle bytes never travel on
   this stream.
2. **Every other RPC presents the token.** Its first message carries the session
   token; that is all the BPA needs to find the session. The four streaming
   calls, `Send`, `Receive`, `Dispatch` and `Forward`, are the data plane: they
   move ADU or bundle bytes as `chunk` messages ended by `last_chunk`. The
   client abandons a transfer with `cancel`; the BPA abandons one by ending
   the call with a status.
3. **Some events expect an RPC back.** A `BundleStatusReport` is self-contained.
   A `Delivery` or `Forwarding` names a bundle by its RFC 9171 bundle id and
   expects the component to open `Receive` or `Forward` with that id; the
   component completes the exchange on that call, with an `ack` or a result. A
   bundle whose exchange never completes is not lost: the BPA keeps it and
   pushes the event again to a later registration.

`Unregister`, or closing the `Subscribe` stream, ends the registration and
invalidates the token. One application session, end to end:

```text
 component                                                       BPA
     |                                                            |
     |  Subscribe  Register ------------------------------------> |  the session opens
     |             <------------------------ Registration (token) |  the first event
     |                                                            |
     |  Send       metadata (token), chunk..., last_chunk ------> |  a data-plane call
     |             <-------------------- SendResponse (bundle id) |
     |                                                            |
     |             <------------------------ Delivery (bundle id) |  an event expecting an RPC back
     |  Receive    metadata (token, bundle id) -----------------> |  the RPC back
     |             <------------------------ chunk..., last_chunk |
     |             ack -----------------------------------------> |  commits the delivery
     |                                                            |
     |  Subscribe  Unregister ----------------------------------> |  the session ends
     |             <------------------------------- (stream ends) |  the token is invalid
```

The APIs differ only in their events and calls.

| API | Events the BPA pushes | Data-plane calls | Other calls |
| --- | --- | --- | --- |
| [`application`] | `Delivery`, `BundleStatusReport` | `Send`, `Receive` | none |
| [`service`] | `Delivery`, `BundleStatusReport` | `Send`, `Receive` | none |
| [`cla`] | `Forwarding` | `Dispatch`, `Forward` | `AddPeer`, `RemovePeer`, `ReportTransferOutcome` |
| [`routing`] | none | none | `AddRoute`, `RemoveRoute` |

Errors are gRPC statuses: a code and a short message, and nothing more. The
code is what a client branches on, and every message the server sends is
written out by hand, so nothing a client sent is ever reflected back at it.
The session token travels in message bodies, so the generated types that
carry it print its length and never its bytes under `Debug`.

# Feature flags

With no features enabled the crate is the contract alone: the generated message
types, the generated tonic clients and servers, and the constants
[`MAX_MESSAGE_SIZE`], [`chunking::DEFAULT_CHUNK_SIZE`],
[`chunking::MAX_CHUNK_SIZE`], [`DEFAULT_MAX_FRAME_SIZE`], [`MAX_TRANSFER_SIZE`]
and [`MAX_LANE_COUNT`]. A component in another language, or a Rust component that
speaks the wire directly, needs nothing more.

- `server`: the BPA side. The `server` module holds one `*ServiceImpl` per API,
  each implementing its generated tonic trait over a
  `hardy_bpa::bpa::BpaRegistration`, for a host to mount on its own tonic
  transport.
- `client`: the component SDK. `client::BpaClient` registers a local `hardy_bpa`
  component (application, service, CLA, or routing agent) against a remote BPA;
  the component never sees gRPC.
- `instrument`: `tracing` spans on the SDK's calls and on each server session.

[rfc9171]: https://www.rfc-editor.org/rfc/rfc9171
*/
#![cfg_attr(any(feature = "client", feature = "server"), doc = "# Examples")]
#![cfg_attr(
    feature = "client",
    doc = r#"
Registering an application with a BPA at a known address:

```no_run
use std::sync::Arc;

use hardy_async::TaskPool;
use hardy_bpa::services::Application;
use hardy_bpv7::eid::Service;
use hardy_proto::client::BpaClient;

# async fn run(application: Arc<dyn Application>) -> Result<(), Box<dyn std::error::Error>> {
let tasks = TaskPool::new();
let client = BpaClient::new("http://[::1]:50051", tasks.clone())?;

let registration = client.register_application(Service::Ipn(7), application).await?;
println!("registered as {}", registration.id());

// The application now runs through its callbacks. When it is time to stop:
tasks.shutdown().await;
registration.await?;
# Ok(())
# }
```
"#
)]
#![cfg_attr(
    feature = "server",
    doc = r#"
Serving the application API of a BPA:

```no_run
use std::sync::Arc;

use hardy_async::TaskPool;
use hardy_bpa::bpa::Bpa;
use hardy_proto::server::ApplicationServiceImpl;
use tonic::transport::Server;

# async fn run(bpa: Arc<Bpa>) -> Result<(), Box<dyn std::error::Error>> {
let tasks = TaskPool::new();
let application = ApplicationServiceImpl::new(bpa, tasks.clone());

Server::builder()
    .add_service(application.into_server())
    .serve("[::1]:50051".parse()?)
    .await?;

tasks.shutdown().await;
# Ok(())
# }
```
"#
)]
// The inline test harness nests the BPA's build future inside its own, which
// under the workspace's unified feature set is deeper than rustc's default.
#![cfg_attr(test, recursion_limit = "256")]
pub mod chunking;
#[cfg(feature = "client")]
pub mod client;
#[cfg(any(feature = "client", feature = "server"))]
mod grammar;
#[cfg(feature = "server")]
mod limits;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
mod timeouts;
#[cfg(any(feature = "client", feature = "server"))]
mod timestamp;
#[cfg(any(feature = "client", feature = "server"))]
mod token;

use crate::chunking::DEFAULT_CHUNK_SIZE;

/// The largest encoded gRPC message, in bytes, that the client SDK sends or
/// accepts on any call.
///
/// The SDK sets it as both the encoding and the decoding limit of every
/// generated client it creates. A `chunk` of [`DEFAULT_CHUNK_SIZE`] bytes fits
/// inside it with room for the rest of the message.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// The HTTP/2 maximum frame size, in bytes, that the SDK's default endpoint
/// settings request.
///
/// It is [`DEFAULT_CHUNK_SIZE`] clamped to the largest frame size HTTP/2
/// permits, so that a chunk crosses the connection in as few frames as the
/// protocol allows. The SDK applies it in
/// `client::BpaClient::default_endpoint`.
pub const DEFAULT_MAX_FRAME_SIZE: u32 = {
    let max = (1u32 << 24) - 1;
    if DEFAULT_CHUNK_SIZE < max as usize {
        DEFAULT_CHUNK_SIZE as u32
    } else {
        max
    }
};

/// The largest transfer size, in bytes, a client may declare.
///
/// A `Send` whose declared ADU size exceeds it, or exceeds what the server's
/// target can address, is refused with `RESOURCE_EXHAUSTED` before any data is
/// read. It bounds the declaration, not the bytes: the BPA's own bundle size
/// limit is what bounds a transfer as it is assembled.
pub const MAX_TRANSFER_SIZE: u64 = 8 * 1024 * 1024 * 1024;

/// The largest `lane_count` a CLA may declare when it registers.
///
/// The server refuses a larger value with `INVALID_ARGUMENT`; the SDK refuses
/// it before anything is sent.
pub const MAX_LANE_COUNT: u32 = 256;

pub mod common {
    //! The `hardy.common.v1` messages shared by the four APIs: the sizes a
    //! server announces at registration.
    //!
    //! The types are generated from `proto/common.proto`, and the schema's
    //! comments, which are the wire contract, appear on each of them.
    tonic::include_proto!("hardy.common.v1");
}

pub mod application {
    //! The `hardy.application.v1` API: an application exchanging ADUs with the BPA.
    //!
    //! The types are generated from `proto/application.proto`, and the schema's
    //! comments, which are the wire contract, appear on each of them. The tonic
    //! client is [`application_service_client::ApplicationServiceClient`]; the
    //! server trait and its wrapper are
    //! [`application_service_server::ApplicationService`] and
    //! [`application_service_server::ApplicationServiceServer`].
    #![cfg_attr(
        any(feature = "client", feature = "server"),
        doc = "\nWith the `client` or `server` feature, [`SendOptions`] converts to and from [`hardy_bpa::services::SendOptions`], and [`hardy_bpa::services::StatusNotify`] converts into [`StatusAssertion`], and `StatusAssertion::notify` converts back, with `Unspecified` as `None`."
    )]
    tonic::include_proto!("hardy.application.v1");

    // The `Debug` of every message in this package that carries the session
    // token, written by `build.rs` in place of the derived one it skipped,
    // printing the token's length and never its bytes.
    include!(concat!(env!("OUT_DIR"), "/hardy.application.v1.debug.rs"));

    /// The conversions between these messages and the BPA's own types, and
    /// the chunked-transfer grammar the streaming messages speak.
    #[cfg(any(feature = "client", feature = "server"))]
    mod conversions {
        use hardy_bpa::services;

        use super::*;
        #[cfg(feature = "server")]
        use crate::grammar::impl_register;
        #[cfg(feature = "client")]
        use crate::grammar::impl_registration;
        use crate::grammar::{impl_ack, impl_cancel, impl_chunk, impl_unregister};

        impl_chunk!(SendRequest, request, send_request::Request);

        impl_cancel!(SendRequest, request, send_request::Request, Cancel);

        impl_chunk!(ReceiveResponse, response, receive_response::Response);

        impl_cancel!(ReceiveRequest, request, receive_request::Request, Cancel);

        impl_ack!(ReceiveRequest, request, receive_request::Request, Ack);

        #[cfg(feature = "server")]
        impl_register!(
            SubscribeRequest,
            request,
            subscribe_request::Request,
            Register
        );

        #[cfg(feature = "client")]
        impl_registration!(
            SubscribeResponse,
            event,
            subscribe_response::Event,
            Registration
        );

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

        impl StatusAssertion {
            /// Returns the BPA's notify kind this assertion names.
            ///
            /// Returns `None` for `STATUS_ASSERTION_UNSPECIFIED`, which asserts
            /// nothing.
            pub fn notify(self) -> Option<services::StatusNotify> {
                match self {
                    Self::Received => Some(services::StatusNotify::Received),
                    Self::Forwarded => Some(services::StatusNotify::Forwarded),
                    Self::Delivered => Some(services::StatusNotify::Delivered),
                    Self::Deleted => Some(services::StatusNotify::Deleted),
                    Self::Unspecified => None,
                }
            }
        }
    }
}

pub mod service {
    //! The `hardy.service.v1` API: a service exchanging complete bundles with the
    //! BPA.
    //!
    //! The types are generated from `proto/service.proto`, and the schema's
    //! comments, which are the wire contract, appear on each of them. The tonic
    //! client is [`service_service_client::ServiceServiceClient`]; the server trait
    //! and its wrapper are [`service_service_server::ServiceService`] and
    //! [`service_service_server::ServiceServiceServer`].
    #![cfg_attr(
        any(feature = "client", feature = "server"),
        doc = "\nWith the `client` or `server` feature, [`hardy_bpa::services::StatusNotify`] converts into [`StatusAssertion`], and `StatusAssertion::notify` converts back, with `Unspecified` as `None`."
    )]
    tonic::include_proto!("hardy.service.v1");

    // The `Debug` of every message in this package that carries the session
    // token, written by `build.rs` in place of the derived one it skipped,
    // printing the token's length and never its bytes.
    include!(concat!(env!("OUT_DIR"), "/hardy.service.v1.debug.rs"));

    /// The conversions between these messages and the BPA's own types, and
    /// the chunked-transfer grammar the streaming messages speak.
    #[cfg(any(feature = "client", feature = "server"))]
    mod conversions {
        use hardy_bpa::services;

        use super::*;
        #[cfg(feature = "server")]
        use crate::grammar::impl_register;
        #[cfg(feature = "client")]
        use crate::grammar::impl_registration;
        use crate::grammar::{impl_ack, impl_cancel, impl_chunk, impl_unregister};

        impl_chunk!(SendRequest, request, send_request::Request);

        impl_cancel!(SendRequest, request, send_request::Request, Cancel);

        impl_chunk!(ReceiveResponse, response, receive_response::Response);

        impl_cancel!(ReceiveRequest, request, receive_request::Request, Cancel);

        impl_ack!(ReceiveRequest, request, receive_request::Request, Ack);

        #[cfg(feature = "server")]
        impl_register!(
            SubscribeRequest,
            request,
            subscribe_request::Request,
            Register
        );

        #[cfg(feature = "client")]
        impl_registration!(
            SubscribeResponse,
            event,
            subscribe_response::Event,
            Registration
        );

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

        impl StatusAssertion {
            /// Returns the BPA's notify kind this assertion names.
            ///
            /// Returns `None` for `STATUS_ASSERTION_UNSPECIFIED`, which asserts
            /// nothing.
            pub fn notify(self) -> Option<services::StatusNotify> {
                match self {
                    Self::Received => Some(services::StatusNotify::Received),
                    Self::Forwarded => Some(services::StatusNotify::Forwarded),
                    Self::Delivered => Some(services::StatusNotify::Delivered),
                    Self::Deleted => Some(services::StatusNotify::Deleted),
                    Self::Unspecified => None,
                }
            }
        }
    }
}

pub mod cla {
    //! The `hardy.cla.v1` API: a convergence-layer adapter passing bundles between
    //! the BPA and a link.
    //!
    //! The types are generated from `proto/cla.proto`, and the schema's comments,
    //! which are the wire contract, appear on each of them. The tonic client is
    //! [`cla_service_client::ClaServiceClient`]; the server trait and its wrapper
    //! are [`cla_service_server::ClaService`] and
    //! [`cla_service_server::ClaServiceServer`].
    #![cfg_attr(
        any(feature = "client", feature = "server"),
        doc = "\nWith the `client` or `server` feature, [`ClaAddress`] converts from [`hardy_bpa::cla::ClaAddress`] and back through `TryFrom`, failing with [`AddressError`], [`ClaAddressType`] converts to and from [`hardy_bpa::cla::ClaAddressType`], failing the same way on `Unspecified`, and [`Acceptance`] converts to and from [`hardy_bpa::cla::Acceptance`], reading `Unspecified` as a refusal."
    )]
    tonic::include_proto!("hardy.cla.v1");

    #[cfg(any(feature = "client", feature = "server"))]
    use hardy_bpa::cla::Error as DomainError;

    // The `Debug` of every message in this package that carries the session
    // token, written by `build.rs` in place of the derived one it skipped,
    // printing the token's length and never its bytes.
    include!(concat!(env!("OUT_DIR"), "/hardy.cla.v1.debug.rs"));

    /// Prints the variant name; the generated `Debug` is skipped along with
    /// its message's.
    impl core::fmt::Debug for report_transfer_outcome_request::Outcome {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str(match self {
                Self::Completed(()) => "Completed",
                Self::Failed(()) => "Failed",
            })
        }
    }

    /// The reason a wire [`ClaAddress`] could not become a
    /// [`hardy_bpa::cla::ClaAddress`].
    #[cfg_attr(
        feature = "server",
        doc = "\nWith the `server` feature it converts into a `tonic::Status` of `INVALID_ARGUMENT` naming the field at fault."
    )]
    #[cfg(any(feature = "client", feature = "server"))]
    #[derive(Debug, thiserror::Error)]
    pub enum AddressError {
        /// The address type was unspecified, or this build does not recognize it.
        #[error("unspecified address type")]
        Unspecified,

        /// The address bytes are not a valid address of the stated type.
        #[error("invalid address")]
        Invalid(#[from] DomainError),
    }

    /// The reason the client SDK refused a CLA registration before anything
    /// was sent: it declared more lanes than the BPA offers.
    #[cfg(feature = "client")]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
    #[error("lane_count {declared} exceeds the maximum of {max}")]
    pub struct LaneCountError {
        /// The lane count the CLA declared.
        pub declared: u32,
        /// The most a CLA may declare, [`MAX_LANE_COUNT`](crate::MAX_LANE_COUNT).
        pub max: u32,
    }

    /// The conversions between these messages and the BPA's own types, and
    /// the chunked-transfer grammar the streaming messages speak.
    #[cfg(any(feature = "client", feature = "server"))]
    mod conversions {
        use hardy_bpa::cla::{
            Acceptance as DomainAcceptance, ClaAddress as DomainClaAddress,
            ClaAddressType as DomainClaAddressType,
        };

        use super::*;
        #[cfg(feature = "server")]
        use crate::grammar::impl_register;
        #[cfg(feature = "client")]
        use crate::grammar::impl_registration;
        use crate::grammar::{impl_cancel, impl_chunk, impl_unregister};

        impl_chunk!(DispatchRequest, request, dispatch_request::Request);

        impl_cancel!(DispatchRequest, request, dispatch_request::Request, Cancel);

        impl_chunk!(ForwardResponse, response, forward_response::Response);

        impl_cancel!(ForwardRequest, request, forward_request::Request, Cancel);

        #[cfg(feature = "server")]
        impl_register!(
            SubscribeRequest,
            request,
            subscribe_request::Request,
            Register
        );

        #[cfg(feature = "client")]
        impl_registration!(
            SubscribeResponse,
            event,
            subscribe_response::Event,
            Registration
        );

        impl_unregister!(SubscribeRequest, request, subscribe_request::Request);

        impl From<DomainAcceptance> for Acceptance {
            fn from(acceptance: DomainAcceptance) -> Self {
                match acceptance {
                    DomainAcceptance::Accepted => Self::Accepted,
                    DomainAcceptance::Refused => Self::Refused,
                }
            }
        }

        /// Converts a wire acceptance into the BPA's, reading anything but an
        /// explicit acceptance as [`Refused`](DomainAcceptance::Refused).
        ///
        /// `ACCEPTANCE_UNSPECIFIED` is proto3's zero, which the schema gives
        /// to a BPA that stated no decision, and the generated getter also
        /// gives to a value this build does not know. Both are verdicts this
        /// side cannot read, and acknowledging to a peer a bundle the BPA
        /// never took is the one outcome a CLA cannot recover from.
        impl From<Acceptance> for DomainAcceptance {
            fn from(acceptance: Acceptance) -> Self {
                match acceptance {
                    Acceptance::Accepted => Self::Accepted,
                    Acceptance::Refused | Acceptance::Unspecified => Self::Refused,
                }
            }
        }

        /// Converts a wire address type into the BPA's.
        ///
        /// # Errors
        ///
        /// Returns [`AddressError::Unspecified`] for
        /// `CLA_ADDRESS_TYPE_UNSPECIFIED`, which names no type.
        impl TryFrom<ClaAddressType> for DomainClaAddressType {
            type Error = AddressError;

            fn try_from(address_type: ClaAddressType) -> Result<Self, AddressError> {
                match address_type {
                    ClaAddressType::Unspecified => Err(AddressError::Unspecified),
                    ClaAddressType::Tcp => Ok(Self::Tcp),
                    ClaAddressType::Private => Ok(Self::Private),
                }
            }
        }

        impl From<DomainClaAddressType> for ClaAddressType {
            fn from(address_type: DomainClaAddressType) -> Self {
                match address_type {
                    DomainClaAddressType::Tcp => Self::Tcp,
                    DomainClaAddressType::Private => Self::Private,
                }
            }
        }

        impl From<DomainClaAddress> for ClaAddress {
            fn from(address: DomainClaAddress) -> Self {
                let (address_type, address) = address.into();
                Self {
                    address_type: ClaAddressType::from(address_type) as i32,
                    address,
                }
            }
        }

        /// Converts a wire address into the BPA's, checking the bytes against the
        /// address type.
        ///
        /// # Errors
        ///
        /// Returns [`AddressError::Unspecified`] if the address type is unspecified or
        /// unknown, and [`AddressError::Invalid`] if the bytes are not a valid address
        /// of that type.
        impl TryFrom<ClaAddress> for DomainClaAddress {
            type Error = AddressError;

            fn try_from(address: ClaAddress) -> Result<Self, AddressError> {
                let address_type = DomainClaAddressType::try_from(address.address_type())?;
                Ok(Self::try_from((address_type, address.address))?)
            }
        }
    }
}

pub mod routing {
    //! The `hardy.routing.v1` API: a routing agent supplying routes to the BPA.
    //!
    //! The types are generated from `proto/routing.proto`, and the schema's
    //! comments, which are the wire contract, appear on each of them. The tonic
    //! client is [`routing_service_client::RoutingServiceClient`]; the
    //! server trait and its wrapper are
    //! [`routing_service_server::RoutingService`] and
    //! [`routing_service_server::RoutingServiceServer`].
    #![cfg_attr(
        any(feature = "client", feature = "server"),
        doc = "\nWith the `client` or `server` feature, [`RouteAction`] converts from a [`hardy_bpa::routing::RouteAction`] reference and [`route_action::Action`] converts into one, each through `TryFrom` and each failing with [`RouteActionError`]."
    )]
    tonic::include_proto!("hardy.routing.v1");

    #[cfg(any(feature = "client", feature = "server"))]
    use hardy_bpv7::eid;

    // The `Debug` of every message in this package that carries the session
    // token, written by `build.rs` in place of the derived one it skipped,
    // printing the token's length and never its bytes.
    include!(concat!(env!("OUT_DIR"), "/hardy.routing.v1.debug.rs"));

    /// The reason a route action could not cross between the wire and
    /// [`hardy_bpa::routing::RouteAction`].
    #[cfg_attr(
        feature = "server",
        doc = "\nWith the `server` feature it converts into a `tonic::Status` of `INVALID_ARGUMENT` naming the field at fault."
    )]
    #[cfg(any(feature = "client", feature = "server"))]
    #[derive(Debug, thiserror::Error)]
    pub enum RouteActionError {
        /// The `via` endpoint id does not parse as an EID.
        #[error("invalid via EID")]
        InvalidVia(#[from] eid::Error),
        /// The drop reason code is 255, which RFC 9171 reserves.
        #[error("reserved status report reason code")]
        ReservedReason,
    }

    /// The conversions between these messages and the BPA's own types, and
    /// the chunked-transfer grammar the streaming messages speak.
    #[cfg(any(feature = "client", feature = "server"))]
    mod conversions {
        use hardy_bpa::routing::RouteAction as DomainRouteAction;
        use hardy_bpv7::{eid::Eid, status_report::ReasonCode};

        use super::*;
        #[cfg(feature = "server")]
        use crate::grammar::impl_register;
        #[cfg(feature = "client")]
        use crate::grammar::impl_registration;
        use crate::grammar::impl_unregister;

        #[cfg(feature = "server")]
        impl_register!(
            SubscribeRequest,
            request,
            subscribe_request::Request,
            Register
        );

        #[cfg(feature = "client")]
        impl_registration!(
            SubscribeResponse,
            event,
            subscribe_response::Event,
            Registration
        );

        impl_unregister!(SubscribeRequest, request, subscribe_request::Request);

        /// Converts a BPA route action into its wire form.
        ///
        /// # Errors
        ///
        /// Returns [`RouteActionError::ReservedReason`] if the action drops with reason
        /// code 255.
        impl TryFrom<&DomainRouteAction> for RouteAction {
            type Error = RouteActionError;

            fn try_from(action: &DomainRouteAction) -> Result<Self, RouteActionError> {
                let action = match action {
                    DomainRouteAction::Drop(reason) => {
                        let reason_code = reason.map(u64::from);
                        if reason_code.is_some_and(|code| ReasonCode::try_from(code).is_err()) {
                            return Err(RouteActionError::ReservedReason);
                        }
                        route_action::Action::Drop(Discard { reason_code })
                    }
                    DomainRouteAction::Reflect => route_action::Action::Reflect(()),
                    DomainRouteAction::Via(eid) => route_action::Action::Via(eid.to_string()),
                };
                Ok(Self {
                    action: Some(action),
                })
            }
        }

        /// Converts a wire route action into the BPA's.
        ///
        /// # Errors
        ///
        /// Returns [`RouteActionError::InvalidVia`] if the `via` endpoint id does not
        /// parse, and [`RouteActionError::ReservedReason`] if the action drops with
        /// reason code 255.
        impl TryFrom<route_action::Action> for DomainRouteAction {
            type Error = RouteActionError;

            fn try_from(action: route_action::Action) -> Result<Self, RouteActionError> {
                Ok(match action {
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
}

#[cfg(test)]
mod tests {
    use prost::bytes::Bytes;

    use super::*;

    #[test]
    fn a_message_carrying_the_token_does_not_print_it() {
        let token = Bytes::from_static(b"application:ipn:7.0123456789abcdef");
        let printed = [
            format!(
                "{:?}",
                application::SendMetadata {
                    session_token: token.clone(),
                    ..Default::default()
                }
            ),
            format!(
                "{:?}",
                application::Registration {
                    session_token: token.clone(),
                    ..Default::default()
                }
            ),
            format!(
                "{:?}",
                service::SendMetadata {
                    session_token: token.clone(),
                    ..Default::default()
                }
            ),
            format!(
                "{:?}",
                cla::AddPeerRequest {
                    session_token: token.clone(),
                    ..Default::default()
                }
            ),
            format!(
                "{:?}",
                routing::AddRouteRequest {
                    session_token: token.clone(),
                    ..Default::default()
                }
            ),
        ];
        for text in printed {
            assert!(
                !text.contains("0123456789abcdef") && text.contains("session_token: 34 bytes"),
                "{text}"
            );
        }
    }
}
