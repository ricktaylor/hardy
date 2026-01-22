mod dispatcher;
mod rib;
mod service_registry;

pub mod bpa;
pub mod bundle;
pub mod cla;
pub mod config;
pub mod metadata;
pub mod node_ids;
pub mod policy;
pub mod routes;
pub mod service;
pub mod storage;

use std::sync::Arc;
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "tracing")]
use tracing::{Instrument, instrument};

// Re-export for consistency
pub use async_trait::async_trait;
pub use bytes::Bytes;
