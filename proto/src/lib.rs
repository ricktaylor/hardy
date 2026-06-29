// Because Prost is too lose with Rustdoc comments
#![allow(clippy::doc_lazy_continuation)]

/*!
gRPC transport layer for the Hardy BPA.

This crate provides client and server implementations that allow BPA
components (CLAs, services, applications, and routing agents) to
communicate with a BPA instance over gRPC. It uses a session-oriented
bidirectional streaming RPC pattern built around [`proxy::RpcProxy`],
which splits each session into independent reader and writer tasks with
message-ID correlation for request/response pairs. Stream closure drives
unregistration, ensuring automatic cleanup on disconnect or crash.

# Modules

- [`client`] -- `RemoteBpa` and per-component client sinks that connect
  to a remote BPA server.
- [`server`] -- gRPC server that exposes a local BPA to remote components,
  with configurable service endpoints.
*/

use hardy_async::sync::spin::Mutex;
use hardy_bpa::async_trait;
use hardy_bpv7::eid;
use std::sync::{Arc, Weak};
use tracing::{debug, error, info, warn};

pub(crate) mod proto;
pub(crate) mod proxy;

/// Cap (in bytes) on a single encoded gRPC message in either direction.
///
/// Both client and server tonic stubs are configured with this value via
/// `max_encoding_message_size` and `max_decoding_message_size`. Sinks
/// pre-check payload size against [`MAX_PAYLOAD_SIZE`] before sending so
/// an oversized message returns a typed error instead of breaking the
/// stream (which would cascade into `on_close` and unregister the
/// application).
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Cap (in bytes) on the payload byte slice a sink will accept. Slightly
/// smaller than [`MAX_MESSAGE_SIZE`] to leave headroom for the protobuf
/// message framing (destination string, options, etc.) so a payload that
/// fits the check also fits the encoded message.
pub const MAX_PAYLOAD_SIZE: usize = MAX_MESSAGE_SIZE - 64 * 1024;

/// Client-side gRPC stubs that present a remote BPA as a local `BpaRegistration`.
pub mod client;
/// Server-side gRPC endpoints that expose a local BPA to remote components.
pub mod server;
