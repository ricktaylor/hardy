// Prost generates doc comments that trip this lint.
#![allow(clippy::doc_lazy_continuation)]

//! gRPC transport for the Hardy BPA.
//!
//! BPA components (CLAs, services, applications and routing agents) run in
//! one process and the BPA in another, talking over a session-oriented
//! bidirectional streaming RPC built around the crate's own `RpcProxy`. Each
//! session splits into independent reader and writer tasks and correlates
//! request and response by message id. Closing the stream unregisters the
//! component, so a disconnect or a crash cleans up on its own.
//!
//! [`client`] holds [`RemoteBpa`](client::RemoteBpa) and the per-component
//! client sinks that connect to a remote BPA server; [`server`] holds the
//! gRPC server that exposes a local BPA to remote components.

use hardy_async::sync::spin::Mutex;
use hardy_bpa::async_trait;
use hardy_bpv7::eid;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

pub(crate) mod proto;
pub(crate) mod proxy;

/// Cap, in bytes, on a single encoded gRPC message in either direction.
///
/// Both the client and the server tonic stubs set this as their
/// `max_encoding_message_size` and `max_decoding_message_size`. Sinks check
/// a payload against [`MAX_PAYLOAD_SIZE`] before sending, so an oversized
/// message returns a typed error instead of breaking the stream, which
/// would cascade into `on_close` and unregister the application.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Cap, in bytes, on the payload byte slice a sink accepts.
///
/// Slightly smaller than [`MAX_MESSAGE_SIZE`], leaving headroom for the
/// protobuf framing around the payload, so a payload that passes the check
/// also fits the encoded message.
pub const MAX_PAYLOAD_SIZE: usize = MAX_MESSAGE_SIZE - 64 * 1024;

/// Client-side gRPC stubs that present a remote BPA through the local
/// registration traits.
pub mod client;
/// Server-side gRPC endpoints that expose a local BPA to remote components.
pub mod server;
