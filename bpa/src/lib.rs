mod cla_registry;
mod dispatcher;
mod rib;
mod sentinel;
mod service_registry;
mod store;

pub mod bpa;
pub mod bundle;
pub mod bundle_mem;
pub mod cla;
pub mod config;
pub mod metadata;
pub mod metadata_mem;
pub mod node_ids;
pub mod routes;
pub mod service;
pub mod storage;

use std::sync::Arc;
use trace_err::*;
use tracing::{Instrument, error, info, trace, warn};

#[cfg(feature = "no-tracing")]
use fuzz_macros::instrument;

#[cfg(not(feature = "no-tracing"))]
use tracing::instrument;

// Re-export for consistency
pub use async_trait::async_trait;
pub use tokio_util::bytes::Bytes;
