mod bundle;
mod bundle_mem;
mod cla_registry;
mod connected;
mod dispatcher;
mod metadata_mem;
mod rib;
mod service_registry;
mod store;

pub mod admin_endpoints;
pub mod bpa;
pub mod cla;
pub mod config;
pub mod metadata;
pub mod routes;
pub mod service;
pub mod storage;

use hardy_bpv7::prelude as bpv7;
use hardy_cbor as cbor;
use hardy_eid_pattern as eid_pattern;
use std::sync::Arc;
use trace_err::*;
use tracing::{error, info, trace, warn};

#[cfg(fuzzing)]
use fuzz_macros::instrument;

#[cfg(not(fuzzing))]
use tracing::instrument;

// Re-export for consistency
pub use async_trait::async_trait;
