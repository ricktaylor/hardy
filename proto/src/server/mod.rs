//! The BPA side of the four APIs, for a host to mount on its own tonic
//! transport.
//!
//! Each `*ServiceImpl` implements the generated tonic trait of one API over a
//! [`BpaRegistration`](hardy_bpa::bpa::BpaRegistration), so that a remote
//! component registers with the BPA as a local one would. A host creates each
//! with the BPA and a [`TaskPool`](hardy_async::TaskPool), wraps it in the
//! generated server type with `into_server`, which sizes it for the wire
//! contract, and adds it to a `tonic::transport::Server`. Every `Subscribe`
//! becomes a session task on the pool, and shutting the pool down ends every
//! session.
//!
//! [`Limits`] bounds what a client can make the server wait for. The BPA offers
//! deliveries and forwardings one at a time per endpoint or peer, so a client
//! that stops answering blocks a queue rather than one bundle; the limits close
//! such a session, and the BPA keeps whatever it held to offer again later.
//!
//! # Examples
//!
//! Serving all four APIs of one BPA:
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use hardy_async::TaskPool;
//! use hardy_bpa::bpa::Bpa;
//! use hardy_proto::server::{
//!     ApplicationServiceImpl, ClaServiceImpl, Limits, RoutingServiceImpl, ServiceServiceImpl,
//! };
//! use tonic::transport::Server;
//!
//! # async fn run(bpa: Arc<Bpa>) -> Result<(), Box<dyn std::error::Error>> {
//! let tasks = TaskPool::new();
//! let limits = Limits::default();
//!
//! Server::builder()
//!     .add_service(ApplicationServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits).into_server())
//!     .add_service(ServiceServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits).into_server())
//!     .add_service(ClaServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits).into_server())
//!     .add_service(RoutingServiceImpl::with_limits(bpa, tasks.clone(), limits).into_server())
//!     .serve("[::1]:50051".parse()?)
//!     .await?;
//!
//! tasks.shutdown().await;
//! # Ok(())
//! # }
//! ```

mod announce;
mod services;
mod status;

pub use self::services::{
    cla::ClaServiceImpl,
    endpoint::{application::ApplicationServiceImpl, service::ServiceServiceImpl},
    routing::RoutingServiceImpl,
};
pub use crate::limits::{
    CLA_SESSION_FOOTPRINT, Limits, MAX_INBOUND_TRANSFERS, MAX_OUTBOUND_FORWARDINGS,
    MAX_SERVER_MESSAGE_SIZE, SESSION_FOOTPRINT,
};
