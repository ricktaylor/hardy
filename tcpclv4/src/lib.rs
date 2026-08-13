/*!
TCPCLv4 Convergence Layer Adapter for the Bundle Protocol.

This crate implements the TCP Convergence-Layer Protocol Version 4 (TCPCLv4)
as defined in [RFC 9174](https://www.rfc-editor.org/rfc/rfc9174). It provides
a [`Tcpclv4`] CLA that registers with the BPA to send and receive bundles
over TCP connections, with optional TLS encryption.

# Key types

- [`Tcpclv4`] — the convergence layer adapter, created via
  [`Tcpclv4::builder`] and registered with a BPA instance through
  `hardy_bpa`'s CLA registration.
- [`builder::Tcpclv4Builder`] — the fluent constructor; every setting has a
  default documented on its method.
- [`tls::Tls`] — TLS material (trust anchor, node identity, mutual-TLS
  policy), chained from [`tls::Tls::builder`] and loaded by
  [`builder::Tcpclv4Builder::build`].
- [`enum@Error`] — the generic error for everything TCPCLv4: assembling
  and registering the entity, with each sub-concept's error wrapped as a
  variant (`Tls`, `Session`).

# Feature flags

- `instrument` — adds `tracing` spans to the async internals.
*/

mod codec;
mod connection;
mod error;
mod otel_metrics;
mod session;
mod tcpclv4;
mod transport;
mod writer;

pub mod builder;
pub mod tls;

pub use self::error::Error;
pub use self::tcpclv4::{ContactTimeout, KeepaliveInterval, Tcpclv4};

use core::num::{NonZeroU32, NonZeroU64};
use hardy_async::sync::spin::Once;
use hardy_bpv7::eid::NodeId;
use std::net::SocketAddr;
use std::sync::Arc;
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "instrument")]
use tracing::instrument;
