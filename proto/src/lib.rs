// Because Prost is too lose with Rustdoc comments
#![allow(clippy::doc_lazy_continuation)]

//! gRPC transport layer for the Hardy BPA.
//!
//! This crate provides client and server implementations that allow BPA
//! components (CLAs, services, applications, and routing agents) to
//! communicate with a BPA instance over gRPC. It uses a session-oriented
//! bidirectional streaming RPC pattern built around [`proxy::RpcProxy`],
//! which splits each session into independent reader and writer tasks with
//! message-ID correlation for request/response pairs. Stream closure drives
//! unregistration, ensuring automatic cleanup on disconnect or crash.
//!
//! # Modules
//!
//! - [`client`] -- `RemoteBpa` and per-component client sinks that connect
//!   to a remote BPA server.
//! - [`server`] -- gRPC server that exposes a local BPA to remote components,
//!   with configurable service endpoints.

use hardy_async::sync::spin::Mutex;
use hardy_bpa::async_trait;
use hardy_bpv7::eid;
use std::sync::{Arc, Weak};
use tracing::{debug, error, info, warn};

pub(crate) mod proto;
pub(crate) mod proxy;

/// Client-side gRPC stubs that present a remote BPA as a local `BpaRegistration`.
pub mod client;
/// Server-side gRPC endpoints that expose a local BPA to remote components.
pub mod server;
