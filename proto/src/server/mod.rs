/*!
The gRPC server: one service per component surface, implementing the
v1 wire contract against the registration traits of `hardy_bpa`.

`Subscribe` opens a session: it registers the component and returns a
stream carrying the registration and then events. Every other RPC
presents the session token minted at registration. Subscriptions run
on the host's `TaskPool`, and every client-driven wait is bounded by
the surface's [`Limits`].
*/

mod adapter;
mod announce;
mod error;
mod leases;
mod services;
mod session;

pub use self::{
    leases::Limits,
    services::{
        application::ApplicationServiceImpl, cla::ClaServiceImpl, routing::RoutingAgentServiceImpl,
        service::ServiceServiceImpl,
    },
};
