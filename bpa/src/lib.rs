mod bundle;
mod bundle_mem;
mod cla_registry;
mod connected;
mod dispatcher;
mod fib_impl;
mod metadata_mem;
mod service_registry;
mod store;
mod utils;

pub mod admin_endpoints;
pub mod bpa;
pub mod cla;
pub mod config;
pub mod fib;
pub mod metadata;
pub mod service;
pub mod storage;

use hardy_bpv7::prelude as bpv7;
use hardy_cbor as cbor;
use hardy_eid_pattern::prelude as eid_pattern;
use std::sync::Arc;
use trace_err::*;
use tracing::{error, info, trace, warn};

#[cfg(fuzzing)]
use fuzz_macros::instrument;

#[cfg(not(fuzzing))]
use tracing::instrument;

// Re-export for consistency
pub use async_trait::async_trait;
