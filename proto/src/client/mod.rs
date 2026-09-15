//! The component SDK: a local component registered against a remote BPA.
//!
//! [`BpaClient`] is the entry point. A component implements the same
//! `hardy_bpa` trait it would implement to register with an in-process BPA
//! ([`Application`](hardy_bpa::services::Application),
//! [`Service`](hardy_bpa::services::Service), [`Cla`](hardy_bpa::cla::Cla) or
//! [`RoutingAgent`](hardy_bpa::routing::RoutingAgent)), and one of the client's
//! `register_*` methods registers it over the wire. The SDK owns the session,
//! its token, its event loop and its data-plane calls; the component's
//! callbacks and the sink it is handed behave as they would locally, and it
//! never sees gRPC. Each registration yields a [`RegistrationHandle`] that
//! resolves when its session ends.
//!
//! [`Endpoint`] is tonic's description of a connection, unrelated to a bundle
//! endpoint id. It is re-exported for callers that configure their own
//! connection and pass it to [`BpaClient::with_endpoint`].

mod bpa_client;
mod services;

pub use self::bpa_client::{BpaClient, EndpointError, RegistrationHandle};
pub use tonic::transport::Endpoint;
